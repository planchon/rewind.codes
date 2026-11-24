//! Every message of the node link through its JSON and back: values that fill every field, in
//! every variant, come back as they went.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::num::NonZeroU32;
use std::ops::Bound as StdBound;
use std::time::Duration;

use rewind_core::{
    ActionFailure, ActionFailureKind, ActionPolicy, ActionRoute, ActionSet, Annotation, Callsite,
    CancelReason, Code, Command, Completion, Digest, Epoch, Event, HistoryEvent, Lifecycle,
    LogLevel, PartitionId, Payload, RunId, RunMode, RunStarted, RunStatus, SecretRef, Seed, Seq,
    Shape, Timestamp,
};
use rewind_engine::metrics::RunMeasurement;
use rewind_engine::ports::link::{
    ActivationRequest, ActivationResult, Admit, Changed, Committed, CountsReport, Decision,
    EdgeNotice, HarnessFailure, HarnessTask, HarnessTaskOffer, NodeReply, NodeRequest, OwnerNotice,
    PartitionReply, PartitionRequest, Purpose, Refusal, RunView, SignalAtOwner, SlotRequest,
    SourceHistory, StartAtOwner, StreamAttach, StreamId, SubscriptionKey, TaskAnswer, TenantReply,
    TenantRequest,
};
use rewind_engine::ports::partition::Activation;
use rewind_engine::ports::{
    ActionCounts, ActionDeclarations, Attempt, AttributeValue, CallTask, CompiledCode,
    CounterDelta, Counters, Created, Day, DayUsage, DueTimer, Ending, Enqueued, FailureCounts,
    ForkSource, Found, Gauges, Hour, InboxItem, IndexedSummary, LatestRun, LogRecord, Month,
    NamespaceName, NodeId, OutboxEntry, OutboxId, Page, ParkKind, ParkReason, PartitionGeneration,
    PayloadRecord, Predicate, Projection, Quarantine, QuarantineKind, QuarantineNote, QuotaAlert,
    QuotaAlertKey, QuotaExceeded, QuotaKind, QuotaScope, Range, RunCounts, RunHead, RunMetadata,
    RunOrder, RunQuery, RunRef, RunSummary, RunWorkflow, Scope, ScriptCounts, SealedAttempt,
    SearchCursor, SecretExpiredReason, Sketch, Span, SpanId, SpanKind, SpanStatus, StartedBy,
    StoredActionSet, StoredCode, TaskPhase, TaskRecord, TaskRef, TaskState, TaskToken, TenantId,
    Term, TextMatch, Threshold, TimerChange, TimerKind, TraceContext, TraceId, Trigger, Usage,
    Wait, WaitKind, WaitingFor, Webhook, WebhookKind, Window,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use super::Invalid;

/// The value through its DTO, its JSON, and back.
fn round_trip<E, D>(value: &E) -> String
where
    E: PartialEq + Debug + TryFrom<D, Error = Invalid>,
    D: for<'a> From<&'a E> + Serialize + DeserializeOwned,
{
    let json = serde_json::to_string(&D::from(value)).unwrap();
    let read: D = serde_json::from_str(&json).unwrap();
    let back = E::try_from(read).unwrap();
    assert_eq!(&back, value, "{json}");
    json
}

const T: Timestamp = Timestamp(1_767_225_600_123);

fn at(ms: u64) -> Timestamp {
    Timestamp(T.0 + ms)
}

fn run_id(n: u128) -> RunId {
    format!("019b7a3c-8f00-8003-8000-{n:012x}").parse().unwrap()
}

fn digest(text: &str) -> Digest {
    Digest::of(text.as_bytes())
}

fn acme() -> TenantId {
    TenantId::new("acme").unwrap()
}

fn user() -> NamespaceName {
    NamespaceName::new("user-1").unwrap()
}

fn run(n: u128) -> RunRef {
    RunRef::new(acme(), run_id(n))
}

fn task_ref(n: u128, seq: u32) -> TaskRef {
    TaskRef {
        run: run(n),
        seq: Seq(seq),
    }
}

fn labels() -> BTreeMap<String, String> {
    BTreeMap::from([("agent".to_owned(), "triage".to_owned())])
}

fn trace_context() -> TraceContext {
    TraceContext {
        traceparent: "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into(),
        tracestate: Some("rewind=1".into()),
    }
}

fn metadata() -> RunMetadata {
    let mut secrets = rewind_core::ServerSecrets::new();
    secrets.insert("github", SecretRef::parse("ns/github").unwrap());
    RunMetadata {
        labels: labels(),
        trace_context: Some(trace_context()),
        source: Some(ForkSource {
            run: run_id(9),
            at: Some(4),
        }),
        workflow: Some(RunWorkflow {
            name: "triage".into(),
            version: digest("v1"),
        }),
        started_by: Some(StartedBy {
            run: run_id(8),
            seq: Seq(2),
            detached: true,
        }),
        allowed_labels: BTreeMap::from([("team".to_owned(), "a".to_owned())]),
        secrets,
    }
}

fn started() -> RunStarted {
    RunStarted {
        code: digest("script"),
        action_set: digest("action_set"),
        input: Payload::Value(json!({ "issue": 42 })),
        seed: Seed::from_bytes([7; 32]),
        epoch: Epoch(1),
        expires_at: at(86_400_000),
        mode: RunMode::Test,
    }
}

fn callsite() -> Callsite {
    Callsite { line: 3, column: 9 }
}

fn waits() -> Vec<Wait> {
    vec![
        Wait {
            kind: WaitKind::Signal,
            seq: Some(Seq(4)),
            name: Some("go".into()),
            since: at(1),
            until: Some(at(3_600_000)),
            callsite: Some(callsite()),
            task: None,
        },
        Wait {
            kind: WaitKind::Timer,
            seq: Some(Seq(5)),
            name: None,
            since: at(2),
            until: Some(at(60_000)),
            callsite: None,
            task: None,
        },
        Wait {
            kind: WaitKind::Call,
            seq: Some(Seq(6)),
            name: Some("srv.a".into()),
            since: at(3),
            until: None,
            callsite: None,
            task: Some(TaskPhase::Approval),
        },
        Wait {
            kind: WaitKind::Harness,
            seq: Some(Seq(7)),
            name: Some("ask".into()),
            since: at(4),
            until: None,
            callsite: None,
            task: Some(TaskPhase::Deferred),
        },
    ]
}

fn head(n: u128) -> RunHead {
    RunHead {
        run: run(n),
        namespace: user(),
        origin: "triage-new-issues".into(),
        suffix_len: 14,
        code: digest("script"),
        action_set: digest("action_set"),
        status: RunStatus::Active,
        epoch: Epoch(1),
        attempt: Attempt(2),
        sealed: Some(Attempt(1)),
        failures: 1,
        unfinished: 2,
        quarantine: Some(Quarantine {
            kind: QuarantineKind::Failures,
            reason: "the activation failed".into(),
            since: at(5),
            retried_by: Some("rewind 0.1.0".into()),
        }),
        waits: waits(),
        expires_at: at(86_400_000),
        history_len: 6,
        metadata: metadata(),
        idempotency_key: Some("start-1".into()),
        created_at: T,
        updated_at: at(6),
        ended_at: Some(at(7)),
    }
}

fn summary(n: u128) -> RunSummary {
    RunSummary {
        run: run(n),
        namespace: user(),
        origin: "triage-new-issues".into(),
        suffix_len: 14,
        code: digest("script"),
        headline: "import { signal } from \"rewind\";".into(),
        action_set: digest("action_set"),
        status: RunStatus::Failed,
        mode: Some(RunMode::Live),
        labels: labels(),
        source: Some(run_id(9)),
        workflow: Some("triage".into()),
        started_by: Some(run_id(8)),
        started_at: Some(T),
        ended_at: Some(at(7)),
        last_activity_at: at(7),
        calls: 3,
        errors: 1,
        history_len: 6,
        waits: waits(),
        wakes_at: Some(at(60_000)),
        ending: Some(Ending {
            error: "TypeError".into(),
            message: "x is undefined".into(),
            line: Some(3),
            signature: "TypeError: <var> is undefined".into(),
            fingerprint: "0123456789abcdef".into(),
        }),
        quarantine: Some(QuarantineNote {
            kind: QuarantineKind::NonDeterminism,
            reason: "the replay diverged".into(),
            since: at(5),
        }),
    }
}

fn indexed(n: u128) -> IndexedSummary {
    IndexedSummary {
        summary: summary(n),
        actions: BTreeSet::from(["srv.a".to_owned(), "ask".to_owned()]),
        call_errors: BTreeSet::from([
            ActionFailureKind::Timeout,
            ActionFailureKind::OutcomeUnknown,
        ]),
    }
}

/// One event of each kind of event.
fn history() -> Vec<HistoryEvent> {
    let events = [
        Event::Lifecycle(Lifecycle::RunStarted(started())),
        Event::Command(Command::ScheduleCall {
            seq: Seq(1),
            action: "srv.a".into(),
            args: Payload::Value(json!({ "x": 1 })),
            callsite: Some(callsite()),
        }),
        Event::Annotation(Annotation::Log {
            level: LogLevel::Warn,
            message: "slow".into(),
            callsite: None,
        }),
        Event::Completion(Completion::CallFailed {
            seq: Seq(1),
            error: ActionFailure {
                kind: ActionFailureKind::Unavailable,
                message: "rate limited".into(),
                data: Some(json!({ "retry_after": 2 })),
            },
        }),
        Event::Completion(Completion::CallCompleted {
            seq: Seq(2),
            result: Payload::Digest(digest("large")),
        }),
        Event::Command(Command::CompleteRun {
            output: Payload::Value(json!(null)),
        }),
    ];
    events
        .into_iter()
        .enumerate()
        .map(|(index, event)| HistoryEvent {
            index: index as u64,
            recorded_at: at(index as u64),
            event,
        })
        .collect()
}

fn inbox() -> Vec<InboxItem> {
    vec![
        InboxItem {
            id: 1,
            trigger: Trigger::RunStarted(started()),
            received_at: T,
        },
        InboxItem {
            id: 2,
            trigger: Trigger::Completion(Completion::SignalReceived {
                name: "go".into(),
                payload: Payload::Value(json!({ "ok": true })),
            }),
            received_at: at(1),
        },
        InboxItem {
            id: 3,
            trigger: Trigger::Completion(Completion::CancelRequested {
                reason: CancelReason::Requested(Some("enough".into())),
            }),
            received_at: at(2),
        },
    ]
}

fn payloads() -> Vec<PayloadRecord> {
    vec![
        PayloadRecord {
            digest: digest("large"),
            size: 70_000,
            preview: "{\"rows\":[…".into(),
            value: None,
        },
        PayloadRecord {
            digest: digest("medium"),
            size: 2_000,
            preview: "{\"text\":\"…".into(),
            value: Some(json!({ "text": "x".repeat(2_000) })),
        },
    ]
}

/// A task in each state.
fn tasks() -> Vec<TaskRecord> {
    let states = [
        TaskState::Approval,
        TaskState::Ready {
            next_delivery_at: at(10),
        },
        TaskState::Claimed {
            owner: "node-a".into(),
            until: at(11),
        },
        TaskState::Deferred { until: at(12) },
        TaskState::Parked(ParkReason {
            kind: ParkKind::SchemaChanged,
            message: "the schema changed".into(),
        }),
        TaskState::Parked(ParkReason {
            kind: ParkKind::CredentialsRefused,
            message: "401".into(),
        }),
        TaskState::Parked(ParkReason {
            kind: ParkKind::SecretMissing,
            message: "no secret".into(),
        }),
    ];
    states
        .into_iter()
        .enumerate()
        .map(|(seq, state)| {
            let route = if seq % 2 == 0 {
                ActionRoute::Mcp {
                    server: "srv".into(),
                }
            } else {
                ActionRoute::Harness {
                    group: Some("crm".into()),
                }
            };
            TaskRecord {
                task: CallTask {
                    task: task_ref(1, seq as u32),
                    action_set: digest("action_set"),
                    action: "srv.a".into(),
                    policy: ActionPolicy::default_for(&route),
                    route,
                    args: Payload::Value(json!({ "seq": seq })),
                    state,
                    delivery: 2,
                    schema_change_approved: seq == 4,
                    created_at: at(seq as u64),
                },
                token_hash: (seq == 3).then(|| digest("token")),
            }
        })
        .collect()
}

/// A timer of each kind, set, and one cancelled.
fn timers() -> Vec<TimerChange> {
    let kinds = [
        TimerKind::Code {
            run: run(1),
            seq: Seq(1),
        },
        TimerKind::CallTimeout {
            run: run(1),
            seq: Seq(2),
        },
        TimerKind::DeferredDeadline {
            run: run(1),
            seq: Seq(3),
        },
        TimerKind::RedeliveryBackoff {
            run: run(1),
            seq: Seq(4),
        },
        TimerKind::RunDeadline { run: run(1) },
        TimerKind::ActivationRetry { run: run(1) },
        TimerKind::RetentionEnd { run: run(1) },
        TimerKind::WebhookRetry {
            tenant: acme(),
            entry: OutboxId(5),
        },
    ];
    let mut changes: Vec<TimerChange> = kinds
        .iter()
        .enumerate()
        .map(|(n, kind)| {
            TimerChange::Set(DueTimer {
                at: at(n as u64 * 1_000),
                kind: kind.clone(),
            })
        })
        .collect();
    changes.extend(kinds.into_iter().map(TimerChange::Cancel));
    changes
}

fn sketch() -> Sketch {
    Sketch::of([3, 40, 900])
}

fn run_counts() -> RunCounts {
    RunCounts {
        started: 4,
        ended: BTreeMap::from([(RunStatus::Completed, 2), (RunStatus::Failed, 1)]),
    }
}

fn counters() -> Counters {
    Counters {
        created: 5,
        runs: run_counts(),
        durations: sketch(),
        cpu: Duration::new(3, 141_592_653),
        workflows: BTreeMap::from([("triage".to_owned(), run_counts())]),
        namespaces: BTreeMap::from([(user(), run_counts())]),
        failures: BTreeMap::from([(
            "0123456789abcdef".to_owned(),
            FailureCounts {
                signature: "TypeError: <var> is undefined".into(),
                error: "TypeError".into(),
                runs: 2,
                scripts: BTreeMap::from([(digest("script"), 2)]),
                lines: BTreeMap::from([(3, 2)]),
                latest: vec![LatestRun {
                    run: run_id(3),
                    ended_at: at(9),
                    status: RunStatus::Failed,
                    message: "x is undefined".into(),
                }],
            },
        )]),
        actions: BTreeMap::from([(
            "srv.a".to_owned(),
            ActionCounts {
                calls: 3,
                completed: 1,
                failed: 2,
                errors: BTreeMap::from([(ActionFailureKind::Timeout, 2)]),
                redeliveries: 1,
                latency: sketch(),
            },
        )]),
        scripts: BTreeMap::from([(
            digest("script"),
            ScriptCounts {
                runs: run_counts(),
                actions: BTreeSet::from(["srv.a".to_owned()]),
                first_started_at: Some(T),
                last_started_at: Some(at(9)),
            },
        )]),
    }
}

fn gauges() -> Gauges {
    Gauges {
        active_runs: 3,
        waiting: BTreeMap::from([(WaitingFor::Signal, 2), (WaitingFor::Parked, 1)]),
        quarantined: 1,
        scripts: BTreeMap::from([(digest("script"), 3)]),
    }
}

fn alert() -> QuotaAlert {
    QuotaAlert {
        key: QuotaAlertKey {
            tenant: acme(),
            namespace: Some(user()),
            quota: QuotaKind::CpuPerMonth,
            period: Month::new(2026, 11).unwrap(),
            threshold: Threshold::Near,
        },
        used: 900,
        limit: 1_000,
        raised_at: at(20),
        sent_at: Some(at(21)),
    }
}

fn projections() -> Vec<Projection> {
    let attributes = BTreeMap::from([
        ("s".to_owned(), AttributeValue::String("v".into())),
        ("i".to_owned(), AttributeValue::Int(-4)),
        ("f".to_owned(), AttributeValue::Float(0.5)),
        ("b".to_owned(), AttributeValue::Bool(true)),
    ]);
    vec![
        Projection::Otel {
            spans: vec![Span {
                trace_id: TraceId([1; 16]),
                span_id: SpanId([2; 8]),
                parent: Some(SpanId([3; 8])),
                name: "action srv.a".into(),
                kind: SpanKind::Client,
                start: T,
                end: at(30),
                status: SpanStatus::Error {
                    message: "timeout".into(),
                },
                attributes: attributes.clone(),
            }],
            logs: vec![LogRecord {
                trace_id: TraceId([1; 16]),
                span_id: Some(SpanId([2; 8])),
                time: at(31),
                level: LogLevel::Error,
                message: "failed".into(),
                attributes,
            }],
        },
        Projection::Webhooks(Webhook {
            id: format!("{}/5", run_id(1)),
            tenant: acme(),
            namespace: Some(user()),
            run: Some(run_id(1)),
            labels: labels(),
            occurred_at: at(32),
            kind: WebhookKind::SecretExpired {
                seq: Seq(2),
                action: "srv.a".into(),
                message: "401".into(),
                reason: SecretExpiredReason::Refused,
                secret: Some(SecretRef::parse("ns/github").unwrap()),
            },
        }),
        Projection::QuotaAlert(alert()),
    ]
}

fn activation() -> Activation {
    Activation {
        run: run(1),
        attempt: Attempt(2),
        events: history(),
        consumed: vec![1, 2],
        payloads: payloads(),
        tasks: tasks(),
        timers: timers(),
        head: head(1),
        summary: indexed(1),
        counters: vec![
            CounterDelta::Hour {
                tenant: acme(),
                namespace: user(),
                hour: Hour::of(T),
                counts: counters(),
            },
            CounterDelta::Shape {
                tenant: acme(),
                namespace: user(),
                action: "srv.a".into(),
                shape: Shape::Union(vec![Shape::Null, Shape::Array(Box::new(Shape::Number))]),
            },
        ],
        outbox: projections()
            .into_iter()
            .enumerate()
            .map(|(id, projection)| OutboxEntry {
                id: OutboxId(id as u64),
                tenant: acme(),
                run: (id < 2).then(|| run_id(1)),
                projection,
            })
            .collect(),
    }
}

fn action_set() -> ActionSet {
    ActionSet::from_harness_actions(["ask", "crm.find"]).unwrap()
}

fn harness_task() -> HarnessTask {
    HarnessTask {
        token: TaskToken(format!("rewind_tk_{}_00ff", run_id(1))),
        task: task_ref(1, 2),
        namespace: user(),
        action: "ask".into(),
        args: json!({ "question": "?" }),
        idempotency_key: "key-1".into(),
        delivery: 2,
        execution_timeout: Some(Duration::from_millis(1_500)),
        labels: labels(),
        trace_context: Some(trace_context()),
    }
}

fn every_refusal() -> Vec<Refusal> {
    vec![
        Refusal::RunNotFound(run_id(1)),
        Refusal::RunEnded(run_id(1)),
        Refusal::Signal("rewind:ended is kept".into()),
        Refusal::Quota(QuotaExceeded {
            quota: QuotaKind::RunsPerMonth,
            scope: QuotaScope::Tenant,
            limit: 10,
            used: 10,
            resets_at: Some(at(40)),
        }),
        Refusal::NamespaceDeleting,
        Refusal::TaskGone,
        Refusal::TaskNotWaiting {
            task: task_ref(1, 2).to_string(),
            state: "claimed",
        },
        Refusal::ResultTooLarge {
            size: 2_000_000,
            max: 1_000_000,
        },
        Refusal::ParentNotStarted(run_id(9)),
        Refusal::Unavailable("the partition is unavailable".into()),
    ]
}

fn every_predicate() -> Vec<Predicate> {
    let times = Range {
        start: StdBound::Included(T),
        end: StdBound::Excluded(at(10)),
    };
    let counts = Range {
        start: StdBound::Excluded(1),
        end: StdBound::Unbounded,
    };
    vec![
        Predicate::Namespace(BTreeSet::from([user()])),
        Predicate::Id(vec!["019b".into()]),
        Predicate::Status(BTreeSet::from([RunStatus::Pending, RunStatus::Aborted])),
        Predicate::Label {
            key: "agent".into(),
            values: vec![
                TextMatch::Any,
                TextMatch::Exact("triage".into()),
                TextMatch::Prefix("tri".into()),
            ],
        },
        Predicate::Script(vec!["ab".into()]),
        Predicate::ActionSet(vec!["cd".into()]),
        Predicate::Action(vec![TextMatch::Prefix("srv.".into())]),
        Predicate::Error {
            names: BTreeSet::from(["TypeError".to_owned()]),
            kinds: BTreeSet::from([ActionFailureKind::Error]),
        },
        Predicate::Waiting(BTreeSet::from([WaitingFor::Approval, WaitingFor::Deferred])),
        Predicate::Signal(vec![TextMatch::Exact("go".into())]),
        Predicate::Started(times),
        Predicate::Ended(times),
        Predicate::Activity(times),
        Predicate::Duration(counts),
        Predicate::Calls(counts),
        Predicate::Errors(counts),
        Predicate::Source(run_id(9)),
        Predicate::Workflow(vec![TextMatch::Any]),
        Predicate::StartedBy(run_id(8)),
        Predicate::Fingerprint(vec!["0123456789abcdef".into()]),
        Predicate::Quarantined,
        Predicate::Mode(vec![RunMode::Live, RunMode::Test]),
        Predicate::Text("undefined".into()),
    ]
}

fn query() -> RunQuery {
    RunQuery {
        tenant: acme(),
        terms: every_predicate()
            .into_iter()
            .enumerate()
            .map(|(n, predicate)| Term {
                negated: n % 2 == 1,
                predicate,
            })
            .collect(),
    }
}

fn namespace_scope() -> Scope {
    Scope::Namespace {
        tenant: acme(),
        namespace: user(),
    }
}

fn every_partition_request() -> Vec<PartitionRequest> {
    vec![
        PartitionRequest::Start(Box::new(StartAtOwner {
            run: run(1),
            namespace: user(),
            origin: "triage-new-issues".into(),
            started: started(),
            metadata: metadata(),
            start_key: Some("start-1".into()),
            headline: "export default async function main() {".into(),
            created_at: T,
        })),
        PartitionRequest::Signal(SignalAtOwner {
            run: run(1),
            name: "go".into(),
            payload: json!(null),
            key: Some("signal-1".into()),
        }),
        PartitionRequest::Cancel {
            run: run(1),
            message: Some("enough".into()),
        },
        PartitionRequest::Release { run: run(1) },
        PartitionRequest::GiveUp { run: run(1) },
        PartitionRequest::Erase { run: run(1) },
        PartitionRequest::Answer {
            tenant: acme(),
            token: harness_task().token,
            answer: TaskAnswer::Complete(json!({ "answer": 42 })),
        },
        PartitionRequest::Answer {
            tenant: acme(),
            token: harness_task().token,
            answer: TaskAnswer::Fail(HarnessFailure {
                message: "no".into(),
                data: Some(json!(null)),
                retryable: true,
            }),
        },
        PartitionRequest::Answer {
            tenant: acme(),
            token: harness_task().token,
            answer: TaskAnswer::Defer {
                timeout: Some(Duration::from_secs(3_600)),
            },
        },
        PartitionRequest::Decide {
            task: task_ref(1, 2),
            decision: Decision::Approve,
        },
        PartitionRequest::Decide {
            task: task_ref(1, 2),
            decision: Decision::Reject(Some("not now".into())),
        },
        PartitionRequest::Decide {
            task: task_ref(1, 2),
            decision: Decision::Reject(None),
        },
        PartitionRequest::SignalUrl {
            token: "rewind_wh_x".into(),
            payload: json!([1, "two"]),
            key: Some("delivery-1".into()),
        },
        PartitionRequest::GetRun { run: run(1) },
        PartitionRequest::Find {
            tenant: acme(),
            handle: "user-1/triage/06bqx3k9f2mt".parse().unwrap(),
        },
        PartitionRequest::History {
            run: run(1),
            from: 3,
            values: true,
        },
        PartitionRequest::Tasks { run: run(1) },
        PartitionRequest::Search {
            scope: namespace_scope(),
            query: query(),
            order: RunOrder::Wakes,
            page: Page::after(
                SearchCursor {
                    key: Some(7),
                    run: run_id(2),
                },
                50,
            ),
        },
        PartitionRequest::Search {
            scope: Scope::Tenant(acme()),
            query: RunQuery::new(acme()),
            order: RunOrder::Started,
            page: Page::first(10),
        },
        PartitionRequest::Counters {
            scope: namespace_scope(),
            window: Window::hours(Hour(10), Hour(12)),
            interval: None,
        },
        PartitionRequest::Counters {
            scope: Scope::Tenant(acme()),
            window: Window::hours(Hour(10), Hour(34)),
            interval: NonZeroU32::new(6),
        },
        PartitionRequest::Usage {
            scope: namespace_scope(),
            month: Month::new(2026, 11).unwrap(),
            days: true,
        },
        PartitionRequest::QuotaAlerts {
            tenant: acme(),
            namespace: Some(user()),
            month: Month::new(2026, 11).unwrap(),
        },
        PartitionRequest::QuotaAlerts {
            tenant: acme(),
            namespace: None,
            month: Month::new(2026, 12).unwrap(),
        },
        PartitionRequest::Shapes {
            tenant: acme(),
            namespace: Some(user()),
            actions: vec!["srv.a".into()],
        },
    ]
}

#[test]
fn every_request_to_an_owner_round_trips() {
    let orders = RunOrder::ALL;
    let mut requests = every_partition_request();
    requests.extend(orders.into_iter().map(|order| PartitionRequest::Search {
        scope: Scope::Tenant(acme()),
        query: RunQuery::new(acme()),
        order,
        page: Page::first(1),
    }));
    for request in requests {
        round_trip::<_, super::NodeRequest>(&NodeRequest::Partition {
            partition: PartitionId::new(3),
            request: Box::new(request),
        });
    }
}

#[test]
fn every_reply_of_an_owner_round_trips_with_every_refusal() {
    let replies = vec![
        PartitionReply::Started(Ok(Created {
            run: run(1),
            created: false,
        })),
        PartitionReply::Enqueued(Ok(Enqueued::Accepted)),
        PartitionReply::Enqueued(Ok(Enqueued::Duplicate)),
        PartitionReply::Enqueued(Ok(Enqueued::RunEnded)),
        PartitionReply::Done(Ok(true)),
        PartitionReply::Answered(Ok(())),
        PartitionReply::Run(Ok(Some(RunView {
            head: head(1),
            summary: Some(summary(1)),
            source: Some("user-1/triage/06bqx3k9f2mt".parse().unwrap()),
            started_by: None,
        }))),
        PartitionReply::Run(Ok(Some(RunView {
            head: head(2),
            summary: None,
            source: None,
            started_by: Some("user-1/digest/06bqx3k9f2mtq".parse().unwrap()),
        }))),
        PartitionReply::Run(Ok(None)),
        PartitionReply::History(Ok(Some(history()))),
        PartitionReply::History(Ok(None)),
        PartitionReply::Tasks(Ok(Some(tasks()))),
        PartitionReply::Found(Ok(Found {
            summaries: vec![summary(1), summary(2)],
            cursor: Some(SearchCursor {
                key: None,
                run: run_id(2),
            }),
        })),
        PartitionReply::Counted(Ok((vec![counters(), Counters::default()], gauges()))),
        PartitionReply::Used(Ok(BTreeMap::from([(
            user(),
            Usage {
                cpu: Duration::new(7, 5),
                runs_started: 4,
                active_runs: 1,
                workflows: 2,
                days: vec![(
                    Day::of(T),
                    DayUsage {
                        cpu: Duration::new(7, 5),
                        runs_started: 4,
                    },
                )],
            },
        )]))),
        PartitionReply::Alerts(Ok(vec![
            alert(),
            QuotaAlert {
                key: QuotaAlertKey {
                    tenant: acme(),
                    namespace: None,
                    quota: QuotaKind::Workflows,
                    period: Month::new(2026, 11).unwrap(),
                    threshold: Threshold::Reached,
                },
                used: 10,
                limit: 10,
                raised_at: at(22),
                sent_at: None,
            },
        ])),
        PartitionReply::Shapes(Ok(BTreeMap::from([(
            "srv.a".to_owned(),
            Shape::Object(BTreeMap::from([(
                "id".to_owned(),
                rewind_core::Field {
                    shape: Shape::String,
                    optional: true,
                },
            )])),
        )]))),
    ];
    for reply in replies {
        round_trip::<_, super::NodeReply>(&NodeReply::Partition(Box::new(reply)));
    }
    for refusal in every_refusal() {
        round_trip::<_, super::NodeReply>(&NodeReply::Partition(Box::new(
            PartitionReply::Answered(Err(refusal.clone())),
        )));
        round_trip::<_, super::NodeReply>(&NodeReply::Tenant(TenantReply::Admission(Err(refusal))));
    }
    round_trip::<_, super::NodeReply>(&NodeReply::Tenant(TenantReply::Admission(Ok(()))));
    round_trip::<_, super::NodeReply>(&NodeReply::Done);
    let json = round_trip::<_, super::NodeReply>(&NodeReply::NotOwner {
        partition: PartitionId::new(5),
    });
    assert_eq!(json, r#"{"not_owner":{"partition":5}}"#);
}

#[test]
fn every_request_and_notice_of_the_organizations_round_trips() {
    let requests = vec![
        TenantRequest::Admit(Admit {
            partition: PartitionId::new(3),
            generation: PartitionGeneration(7),
            run: run_id(1),
            namespace: user(),
            code: digest("script"),
            at: T,
        }),
        TenantRequest::Counts(CountsReport {
            partition: PartitionId::new(3),
            generation: PartitionGeneration(7),
            namespace: user(),
            gauges: gauges(),
            created_this_month: 4,
            cpu_this_month: Duration::new(12, 345),
            included: vec![run_id(1)],
            abandoned: vec![run_id(2)],
            at: T,
        }),
        // What a partition keeps of its tenants' erased namespaces is reported as theirs.
        TenantRequest::Counts(CountsReport {
            partition: PartitionId::new(3),
            generation: PartitionGeneration(7),
            namespace: NamespaceName::erased(PartitionId::new(3)),
            gauges: gauges(),
            created_this_month: 2,
            cpu_this_month: Duration::new(1, 0),
            included: Vec::new(),
            abandoned: Vec::new(),
            at: T,
        }),
        TenantRequest::Slot(SlotRequest {
            partition: PartitionId::new(3),
            generation: PartitionGeneration(7),
            task: task_ref(1, 2),
            server: "srv".into(),
            limit: 4,
        }),
        TenantRequest::SlotFreed {
            task: task_ref(1, 2),
            server: "srv".into(),
        },
        TenantRequest::HarnessTask(Box::new(HarnessTaskOffer {
            partition: PartitionId::new(3),
            generation: PartitionGeneration(7),
            task: harness_task(),
            action_set: digest("action_set"),
            grace: Duration::from_secs(60),
        })),
        TenantRequest::HarnessTaskDone {
            task: task_ref(1, 2),
            gone: false,
        },
        TenantRequest::HarnessTaskDone {
            task: task_ref(1, 3),
            gone: true,
        },
        TenantRequest::Attach(StreamAttach {
            stream: StreamId("stream-1".into()),
            worker: "worker-1".into(),
            node: NodeId::new("node-a").unwrap(),
            namespace: Some(user()),
            labels: labels(),
            action_set: Some(digest("action_set")),
            actions: BTreeSet::from(["ask".to_owned()]),
            slots: 4,
        }),
        TenantRequest::Detach {
            stream: StreamId("stream-1".into()),
        },
    ];
    for request in requests {
        round_trip::<_, super::NodeRequest>(&NodeRequest::Tenant {
            tenant: acme(),
            request: Box::new(request),
        });
    }
    let notices = vec![
        OwnerNotice::SlotGranted {
            task: task_ref(1, 2),
        },
        OwnerNotice::StreamLost {
            tasks: vec![(task_ref(1, 2), harness_task().token)],
        },
        OwnerNotice::CountsWanted { tenant: acme() },
    ];
    for notice in notices {
        round_trip::<_, super::NodeRequest>(&NodeRequest::Owner {
            partition: PartitionId::new(3),
            notice: Box::new(notice),
        });
    }
    round_trip::<_, super::NodeRequest>(&NodeRequest::Edge(Box::new(EdgeNotice::HandOut {
        stream: StreamId("stream-1".into()),
        task: harness_task(),
    })));
    round_trip::<_, super::NodeRequest>(&NodeRequest::Edge(Box::new(EdgeNotice::Gone {
        stream: StreamId("stream-1".into()),
        token: harness_task().token,
    })));
    round_trip::<_, super::NodeRequest>(&NodeRequest::Edge(Box::new(EdgeNotice::Reopened {
        partition: PartitionId::new(7),
    })));
}

#[test]
fn subscriptions_and_changes_round_trip() {
    for key in [
        SubscriptionKey::Run(run(1)),
        SubscriptionKey::Namespace {
            tenant: acme(),
            namespace: user(),
        },
        SubscriptionKey::Tenant(acme()),
    ] {
        round_trip::<_, super::SubscriptionKey>(&key);
    }
    round_trip::<_, super::Changed>(&Changed {
        run: run(1),
        namespace: user(),
        summary: Some(summary(1)),
        events: history(),
    });
    round_trip::<_, super::Changed>(&Changed {
        run: run(1),
        namespace: user(),
        summary: None,
        events: Vec::new(),
    });
}

fn activation_request(purpose: Purpose) -> ActivationRequest {
    ActivationRequest {
        partition: PartitionId::new(3),
        head: head(1),
        summary: Some(indexed(1)),
        inbox: inbox(),
        history: Some(history()),
        payloads: payloads(),
        source: Some(SourceHistory {
            run: run_id(9),
            events: history(),
            payloads: payloads(),
            sealed: Some(Box::new(SealedAttempt {
                run: RunRef::new(acme(), run_id(9)),
                attempt: Attempt(1),
                code: Code::new("export default async () => 1;"),
                action_set: action_set(),
                events: history(),
            })),
        }),
        code: StoredCode {
            code: Code::new("export default async () => 1;"),
            compiled: CompiledCode {
                javascript: "export default async () => 1;".into(),
                source_map: "{\"version\":3}".into(),
            },
        },
        action_set: StoredActionSet {
            action_set: action_set(),
            declarations: ActionDeclarations {
                typescript: "declare module \"rewind:actions\" {}".into(),
                prompt: "ask(question) → unknown".into(),
            },
        },
        purpose,
        tasks: tasks(),
    }
}

#[test]
fn activations_round_trip() {
    for purpose in [Purpose::Runnable, Purpose::QuarantineRetry, Purpose::GiveUp] {
        round_trip::<_, super::ActivationRequest>(&activation_request(purpose));
    }
    let mut cached = activation_request(Purpose::Runnable);
    cached.history = None;
    cached.source = None;
    cached.summary = None;
    round_trip::<_, super::ActivationRequest>(&cached);

    let results = vec![
        ActivationResult::Committed(Box::new(Committed {
            activation: activation(),
            projections: projections(),
            end: Some(json!(null)),
            children: vec![(Seq(1), run_id(4), true), (Seq(2), run_id(5), false)],
            copied: true,
            cpu: Duration::new(0, 1_234_567),
            mode: Some(RunMode::Test),
            measurements: vec![
                RunMeasurement::Started,
                RunMeasurement::Ended {
                    outcome: "completed".into(),
                },
                RunMeasurement::Call {
                    action: "srv.a".into(),
                    outcome: "failed".into(),
                    error_kind: Some("timeout".into()),
                    seconds: 1.5,
                },
                RunMeasurement::Call {
                    action: "ask".into(),
                    outcome: "no_outcome".into(),
                    error_kind: None,
                    seconds: 0.0,
                },
            ],
        })),
        ActivationResult::Committed(Box::new(Committed {
            activation: activation(),
            projections: Vec::new(),
            end: None,
            children: Vec::new(),
            copied: false,
            cpu: Duration::ZERO,
            mode: None,
            measurements: Vec::new(),
        })),
        ActivationResult::Quarantined {
            kind: QuarantineKind::NonDeterminism,
            reason: "the replay diverged at event 3".into(),
        },
        ActivationResult::Released,
        ActivationResult::Failed {
            error: "the executor stopped".into(),
        },
        ActivationResult::Miss,
    ];
    for result in results {
        round_trip::<_, super::ActivationResult>(&result);
    }
}

#[test]
fn what_the_engine_refuses_is_refused() {
    let json = serde_json::to_string(&super::NodeRequest::from(&NodeRequest::Tenant {
        tenant: acme(),
        request: Box::new(TenantRequest::Detach {
            stream: StreamId("s".into()),
        }),
    }))
    .unwrap();
    let bad_tenant = json.replace("\"acme\"", "\"not a tenant\"");
    let read: super::NodeRequest = serde_json::from_str(&bad_tenant).unwrap();
    assert!(NodeRequest::try_from(read).is_err());

    let refusal =
        r#"{"partition":{"answered":{"Err":{"task_not_waiting":{"task":"x","state":"lost"}}}}}"#;
    let read: super::NodeReply = serde_json::from_str(refusal).unwrap();
    let error = NodeReply::try_from(read).unwrap_err();
    assert!(error.to_string().contains("lost"), "{error}");

    let unknown = r#"{"partition":{"partition":1,"request":{"get_run":{"run":{"tenant":"acme","run":"x"}}}}}"#;
    assert!(serde_json::from_str::<super::NodeRequest>(unknown).is_err());
    assert!(serde_json::from_str::<super::NodeReply>(r#"{"later":{}}"#).is_err());
}
