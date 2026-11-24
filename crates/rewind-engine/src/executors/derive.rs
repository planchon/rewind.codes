//! What an activation writes beside its events, derived from the history it extends
//! (`archi/05-testing-and-observability.md`, "Read model", "The projector";
//! `archi/06-storage-and-scale.md`, "Quotas"): the run's head and summary, the call tasks and
//! timers of its new commands, the counters it moves, and the projections it feeds.
//!
//! Everything here is pure: the same history gives the same records on every node, so an
//! activation run by a remote executor commits what a local one would.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::{
    ActionSet, Command, Completion, Event, HistoryEvent, JsonValue, Lifecycle, Payload, RunState,
    RunStatus, Seq, Shape, Timestamp,
};

use crate::failures;
use crate::metrics::RunMeasurement;
use crate::ports::{
    ActionCounts, CallTask, CounterDelta, Counters, DueTimer, FailureCounts, Hour, IndexedSummary,
    LatestRun, PayloadRecord, Projection, QuarantineNote, RunHead, RunSummary, TaskPhase,
    TaskRecord, TaskRef, TaskState, TimerChange, TimerKind, WaitKind,
};
use crate::projector;
use crate::telemetry::{RunTelemetry, SpanKey, TelemetryConfig};

/// The phase a task's records give it, as the summary shows it.
pub(crate) fn phase_of(task: &TaskRecord) -> TaskPhase {
    match task.task.state {
        TaskState::Approval => TaskPhase::Approval,
        TaskState::Ready { .. } => TaskPhase::Ready,
        TaskState::Claimed { .. } => TaskPhase::Claimed,
        TaskState::Deferred { .. } => TaskPhase::Deferred,
        TaskState::Parked(_) => TaskPhase::Parked,
    }
}

/// The run's summary after `history`, which leaves the run in `state`: its status, its waits
/// (each call's with its task's phase), its ending, its counts, the actions it called.
///
/// The counts are those of `previous`, the summary of the history up to its `history_len`,
/// with the events after it: an activation counts its new events only, and its derivation
/// costs what it appends and what the run holds pending, never the length of the history.
/// Without a previous summary (a fork's first activation, a run given up) every event is
/// counted.
pub(crate) fn summary_of(
    head: &RunHead,
    headline: &str,
    history: &[HistoryEvent],
    state: &RunState,
    action_set: &ActionSet,
    tasks: &BTreeMap<Seq, TaskRecord>,
    previous: Option<&IndexedSummary>,
) -> IndexedSummary {
    let mut waits = if state.status() == RunStatus::Active {
        projector::waits(history, state, Some(action_set))
    } else {
        Vec::new()
    };
    for wait in &mut waits {
        if matches!(wait.kind, WaitKind::Call | WaitKind::Harness)
            && let Some(seq) = wait.seq
        {
            wait.task = tasks.get(&seq).map(phase_of);
        }
    }
    let wakes_at = waits.iter().filter_map(|wait| wait.until).min();
    let counted = previous
        .and_then(|previous| usize::try_from(previous.summary.history_len).ok())
        .filter(|counted| *counted <= history.len());
    let (mut actions, mut call_errors, mut calls, mut errors) = match previous.zip(counted) {
        Some((previous, _)) => (
            previous.actions.clone(),
            previous.call_errors.clone(),
            previous.summary.calls,
            previous.summary.errors,
        ),
        None => (BTreeSet::new(), BTreeSet::new(), 0, 0),
    };
    for entry in &history[counted.unwrap_or(0)..] {
        match &entry.event {
            Event::Command(Command::ScheduleCall { action, .. }) => {
                calls += 1;
                actions.insert(action.clone());
            }
            Event::Completion(Completion::CallFailed { error, .. }) => {
                errors += 1;
                call_errors.insert(error.kind);
            }
            _ => {}
        }
    }
    let started_at = history.first().and_then(|entry| match entry.event {
        Event::Lifecycle(Lifecycle::RunStarted(_)) => Some(entry.recorded_at),
        _ => None,
    });
    // Nothing follows the terminal event of an attempt.
    let ended_at = history
        .last()
        .filter(|entry| entry.event.is_terminal())
        .map(|entry| entry.recorded_at);
    let metadata = &head.metadata;
    IndexedSummary {
        summary: RunSummary {
            run: head.run.clone(),
            namespace: head.namespace.clone(),
            origin: head.origin.clone(),
            suffix_len: head.suffix_len,
            code: head.code,
            headline: headline.to_owned(),
            action_set: head.action_set,
            status: if history.is_empty() {
                RunStatus::Pending
            } else {
                state.status()
            },
            mode: state.mode(),
            labels: metadata.labels.clone(),
            source: metadata.source.map(|parent| parent.run),
            workflow: metadata.workflow.as_ref().map(|of| of.name.clone()),
            started_by: metadata.started_by.as_ref().map(|by| by.run),
            started_at,
            ended_at,
            last_activity_at: history
                .last()
                .map_or(head.updated_at, |entry| entry.recorded_at),
            calls,
            errors,
            history_len: history.len() as u64,
            waits,
            wakes_at,
            ending: state.outcome().and_then(failures::ending),
            quarantine: head.quarantine.as_ref().map(QuarantineNote::from),
        },
        actions,
        call_errors,
    }
}

/// The summary of a run created and not activated yet: `pending`.
pub(crate) fn pending_summary(head: &RunHead, headline: &str) -> IndexedSummary {
    let mut summary = summary_of(
        head,
        headline,
        &[],
        &RunState::new(),
        &ActionSet::empty(),
        &BTreeMap::new(),
        None,
    );
    summary.summary.status = RunStatus::Pending;
    summary
}

/// The events that opened the calls and timers the new events (`from` on) end or read:
/// pending before them, read at their index, or opened among them.
pub(crate) struct Openings<'h> {
    history: &'h [HistoryEvent],
    before: &'h RunState,
    new: BTreeMap<Seq, &'h HistoryEvent>,
}

impl<'h> Openings<'h> {
    /// `before` is the run's state after the events before `from`.
    pub(crate) fn new(history: &'h [HistoryEvent], from: usize, before: &'h RunState) -> Self {
        let new = history[from.min(history.len())..]
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::Command(
                    Command::ScheduleCall { seq, .. } | Command::StartTimer { seq, .. },
                ) => Some((*seq, entry)),
                _ => None,
            })
            .collect();
        Self {
            history,
            before,
            new,
        }
    }

    /// The event that opened the call or timer `seq`.
    pub(crate) fn of(&self, seq: Seq) -> Option<&'h HistoryEvent> {
        self.new
            .get(&seq)
            .copied()
            .or_else(|| projector::opening(self.history, self.before, seq))
    }

    /// The call `seq`: when it was scheduled, and its action.
    pub(crate) fn call(&self, seq: Seq) -> Option<(Timestamp, &'h str)> {
        let entry = self.of(seq)?;
        match &entry.event {
            Event::Command(Command::ScheduleCall { action, .. }) => {
                Some((entry.recorded_at, action.as_str()))
            }
            _ => None,
        }
    }
}

/// The run's head after an activation that left it in `status`.
pub(crate) fn head_after(
    head: &RunHead,
    history: &[HistoryEvent],
    summary: &IndexedSummary,
    released: bool,
) -> RunHead {
    let mut next = head.clone();
    next.status = summary.summary.status;
    next.waits = summary.summary.waits.clone();
    next.history_len = history.len() as u64;
    next.failures = 0;
    next.attempts = 0;
    if let Some(last) = history.last() {
        next.updated_at = last.recorded_at;
    }
    if next.status.is_terminal() {
        next.ended_at = summary.summary.ended_at;
        next.quarantine = None;
        next.waits.clear();
    } else if released {
        next.quarantine = None;
    }
    next
}

/// The call tasks and timers of the new events (`from` on): a call still pending at the end of
/// the activation, a timer still pending, the cancellation of a timer of an earlier activation,
/// and the run's deadline once `run_started` is committed. A terminal run has none: its commit
/// clears them. A task takes its route and policy from the pinned action set; an action the action
/// set does not have is returned as the error.
pub(crate) fn effects_of(
    head: &RunHead,
    history: &[HistoryEvent],
    from: usize,
    state: &rewind_core::RunState,
    action_set: &ActionSet,
    now: Timestamp,
) -> Result<(Vec<TaskRecord>, Vec<TimerChange>), String> {
    let mut tasks = Vec::new();
    let mut timers = Vec::new();
    if state.is_terminal() {
        return Ok((tasks, timers));
    }
    let run = &head.run;
    let mut started_here = BTreeSet::new();
    for entry in &history[from.min(history.len())..] {
        match &entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(started)) => {
                timers.push(TimerChange::Set(DueTimer {
                    at: started.expires_at,
                    kind: TimerKind::RunDeadline { run: run.clone() },
                }));
            }
            Event::Command(Command::ScheduleCall {
                seq, action, args, ..
            }) if state.pending_calls().contains_key(seq) => {
                let pinned = action_set.action(action).ok_or_else(|| action.clone())?;
                tasks.push(TaskRecord {
                    task: CallTask {
                        task: TaskRef {
                            run: run.clone(),
                            seq: *seq,
                        },
                        action_set: head.action_set,
                        action: action.clone(),
                        route: pinned.route.clone(),
                        args: args.clone(),
                        policy: pinned.policy,
                        state: if pinned.policy.approval {
                            TaskState::Approval
                        } else {
                            TaskState::Ready {
                                next_delivery_at: now,
                            }
                        },
                        delivery: 1,
                        schema_change_approved: false,
                        created_at: now,
                    },
                    token_hash: None,
                });
            }
            Event::Command(Command::StartTimer { seq, fire_at, .. }) => {
                started_here.insert(*seq);
                if state.pending_timers().contains_key(seq) {
                    timers.push(TimerChange::Set(DueTimer {
                        at: *fire_at,
                        kind: TimerKind::Code {
                            run: run.clone(),
                            seq: *seq,
                        },
                    }));
                }
            }
            Event::Command(Command::CancelTimer { seq }) if !started_here.contains(seq) => {
                timers.push(TimerChange::Cancel(TimerKind::Code {
                    run: run.clone(),
                    seq: *seq,
                }));
            }
            _ => {}
        }
    }
    Ok((tasks, timers))
}

/// The shapes of the results of the new events, before their payloads leave them.
pub(crate) fn shapes_of(
    head: &RunHead,
    history: &[HistoryEvent],
    from: usize,
    openings: &Openings<'_>,
) -> Vec<CounterDelta> {
    let mut deltas = Vec::new();
    for entry in &history[from.min(history.len())..] {
        if let Event::Completion(Completion::CallCompleted {
            seq,
            result: Payload::Value(value),
        }) = &entry.event
            && let Some((_, action)) = openings.call(*seq)
        {
            deltas.push(CounterDelta::Shape {
                tenant: head.run.tenant.clone(),
                namespace: head.namespace.clone(),
                action: action.to_owned(),
                shape: Shape::of(value),
            });
        }
    }
    deltas
}

/// The counters the new events (`from` on) move, per hour of their `recorded_at`: the run's
/// start and end, its duration and failure group, its workflow and code, the calls of each
/// action; and the CPU time of the activation in the hour of `now`.
pub(crate) fn counters_of(
    head: &RunHead,
    history: &[HistoryEvent],
    from: usize,
    openings: &Openings<'_>,
    summary: &IndexedSummary,
    cpu: Duration,
    now: Timestamp,
) -> Vec<CounterDelta> {
    let mut hours: BTreeMap<Hour, Counters> = BTreeMap::new();
    let workflow = head.metadata.workflow.as_ref().map(|of| of.name.clone());
    let started_at = summary.summary.started_at;
    for entry in &history[from.min(history.len())..] {
        match &entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(_)) => {
                let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
                counters.runs.started += 1;
                if let Some(workflow) = &workflow {
                    counters
                        .workflows
                        .entry(workflow.clone())
                        .or_default()
                        .started += 1;
                }
                let code = counters.scripts.entry(head.code).or_default();
                code.runs.started += 1;
                code.first_started_at = Some(entry.recorded_at);
                code.last_started_at = Some(entry.recorded_at);
            }
            Event::Command(Command::ScheduleCall { action, .. }) => {
                let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
                counters.actions.entry(action.clone()).or_default().calls += 1;
                counters
                    .scripts
                    .entry(head.code)
                    .or_default()
                    .actions
                    .insert(action.clone());
            }
            Event::Completion(Completion::CallCompleted { seq, .. }) => {
                if let Some((at, action)) = openings.call(*seq) {
                    let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
                    let calls = counters.actions.entry(action.to_owned()).or_default();
                    calls.completed += 1;
                    calls
                        .latency
                        .record(entry.recorded_at.0.saturating_sub(at.0));
                }
            }
            Event::Completion(Completion::CallFailed { seq, error }) => {
                if let Some((at, action)) = openings.call(*seq) {
                    let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
                    let calls = counters.actions.entry(action.to_owned()).or_default();
                    calls.failed += 1;
                    *calls.errors.entry(error.kind).or_default() += 1;
                    calls
                        .latency
                        .record(entry.recorded_at.0.saturating_sub(at.0));
                }
            }
            _ => {}
        }
        if entry.event.is_terminal() {
            let status = summary.summary.status;
            let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
            *counters.runs.ended.entry(status).or_default() += 1;
            if let Some(started) = started_at {
                counters
                    .durations
                    .record(entry.recorded_at.0.saturating_sub(started.0));
            }
            if let Some(workflow) = &workflow {
                *counters
                    .workflows
                    .entry(workflow.clone())
                    .or_default()
                    .ended
                    .entry(status)
                    .or_default() += 1;
            }
            *counters
                .scripts
                .entry(head.code)
                .or_default()
                .runs
                .ended
                .entry(status)
                .or_default() += 1;
            if let Some(ending) = &summary.summary.ending {
                counters.failures.insert(
                    ending.fingerprint.clone(),
                    FailureCounts {
                        signature: ending.signature.clone(),
                        error: ending.error.clone(),
                        runs: 1,
                        scripts: BTreeMap::from([(head.code, 1)]),
                        lines: ending.line.map(|line| (line, 1)).into_iter().collect(),
                        latest: vec![LatestRun {
                            run: head.run.id,
                            ended_at: entry.recorded_at,
                            status,
                            message: ending.message.clone(),
                        }],
                    },
                );
            }
        }
    }
    if !cpu.is_zero() {
        hours.entry(Hour::of(now)).or_default().cpu += cpu;
    }
    hours
        .into_iter()
        .filter(|(_, counts)| !counts.is_empty())
        .map(|(hour, counts)| CounterDelta::Hour {
            tenant: head.run.tenant.clone(),
            namespace: head.namespace.clone(),
            hour,
            counts,
        })
        .collect()
}

/// The counters of a run created: what `runsPerMonth` counts.
pub(crate) fn created_counters(head: &RunHead) -> CounterDelta {
    CounterDelta::Hour {
        tenant: head.run.tenant.clone(),
        namespace: head.namespace.clone(),
        hour: Hour::of(head.created_at),
        counts: Counters {
            created: 1,
            ..Counters::default()
        },
    }
}

/// The redeliveries of a call that ended after its first delivery.
pub(crate) fn redeliveries_counters(
    head: &RunHead,
    action: &str,
    delivery: u32,
    now: Timestamp,
) -> Option<CounterDelta> {
    let redeliveries = u64::from(delivery.checked_sub(1)?);
    if redeliveries == 0 {
        return None;
    }
    Some(CounterDelta::Hour {
        tenant: head.run.tenant.clone(),
        namespace: head.namespace.clone(),
        hour: Hour::of(now),
        counts: Counters {
            actions: BTreeMap::from([(
                action.to_owned(),
                ActionCounts {
                    redeliveries,
                    ..ActionCounts::default()
                },
            )]),
            ..Counters::default()
        },
    })
}

/// The new events of an activation: `history[from..]`, which take the run from the state
/// `before` to the state `after`.
#[derive(Clone, Copy)]
pub(crate) struct NewEvents<'h> {
    pub(crate) history: &'h [HistoryEvent],
    pub(crate) from: usize,
    pub(crate) before: &'h RunState,
    pub(crate) after: &'h RunState,
}

/// What the new events feed the projections: the spans and logs that end in them, the log
/// records named by the activation span `H(run, "activation", from)`, and the lifecycle
/// webhooks.
pub(crate) fn projections_of(
    head: &RunHead,
    new: NewEvents<'_>,
    action_set: &ActionSet,
    payloads: &BTreeMap<rewind_core::Digest, PayloadRecord>,
    telemetry: &TelemetryConfig,
) -> Vec<Projection> {
    let NewEvents {
        history,
        from,
        before,
        after,
    } = new;
    if history.len() <= from {
        return Vec::new();
    }
    let run_telemetry = RunTelemetry {
        head,
        config: telemetry,
        payloads,
    };
    let (start, end) = (from as u64, history.len() as u64);
    let mut projections = Vec::new();
    if run_telemetry.exported(history) {
        let spans = run_telemetry.spans(history, before, start, end);
        let logs = run_telemetry.logs(history, start, end, SpanKey::Activation(start));
        if !spans.is_empty() || !logs.is_empty() {
            projections.push(Projection::Otel { spans, logs });
        }
    }
    for event in projector::lifecycle_events(head, Some(action_set), history, from, after) {
        projections.push(Projection::Webhooks(event));
    }
    projections
}

/// What the new events measure: the run metrics recorded once their batch is durable, for
/// every run but a test run whose telemetry is not exported.
pub(crate) fn measurements_of(
    head: &RunHead,
    new: NewEvents<'_>,
    payloads: &BTreeMap<rewind_core::Digest, PayloadRecord>,
    telemetry: &TelemetryConfig,
) -> Vec<RunMeasurement> {
    let NewEvents {
        history,
        from,
        before,
        ..
    } = new;
    if history.len() <= from {
        return Vec::new();
    }
    let run_telemetry = RunTelemetry {
        head,
        config: telemetry,
        payloads,
    };
    if !run_telemetry.measured(history) {
        return Vec::new();
    }
    run_telemetry.measurements(history, before, from as u64, history.len() as u64)
}

/// The runs a history started with a workflow's `start` and whose end it does not hold, by
/// call, and whether the start was attached (not `detached`).
pub(crate) fn children_of(history: &[HistoryEvent]) -> Vec<(Seq, rewind_core::RunId, bool)> {
    let mut detached: BTreeMap<Seq, bool> = BTreeMap::new();
    for entry in history {
        if let Event::Command(Command::ScheduleCall {
            seq, action, args, ..
        }) = &entry.event
            && matches!(
                rewind_core::WorkflowOp::of_action(action),
                Some((_, rewind_core::WorkflowOp::Start))
            )
        {
            let flag = args
                .value()
                .and_then(|args| args.get("detached"))
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            detached.insert(*seq, flag);
        }
    }
    crate::workflows::started_runs(history)
        .into_iter()
        .map(|(seq, run)| (seq, run, !detached.get(&seq).copied().unwrap_or(false)))
        .collect()
}

/// The note of a quarantine a summary shows.
pub(crate) fn with_quarantine(
    mut summary: IndexedSummary,
    quarantine: Option<&crate::ports::Quarantine>,
) -> IndexedSummary {
    summary.summary.quarantine = quarantine.map(QuarantineNote::from);
    summary
}

/// The summary with the phases of the run's tasks in its waits.
pub(crate) fn with_phases(
    mut summary: IndexedSummary,
    phases: &BTreeMap<Seq, TaskPhase>,
) -> IndexedSummary {
    for wait in &mut summary.summary.waits {
        if matches!(wait.kind, WaitKind::Call | WaitKind::Harness)
            && let Some(seq) = wait.seq
        {
            wait.task = phases.get(&seq).copied();
        }
    }
    summary
}

#[cfg(test)]
mod tests {
    use rewind_core::{
        Action, ActionFailure, ActionFailureKind, ActionRoute, Annotation, Callsite, CancelReason,
        Digest, Epoch, ErrorInfo, LogLevel, McpServer, McpTransport, RunId, RunMode, RunStarted,
        Seed,
    };
    use serde_json::json;

    use super::*;
    use crate::ports::{Attempt, NamespaceName, RunMetadata, RunRef, TenantId};

    fn head() -> RunHead {
        RunHead {
            run: RunRef::new(TenantId::local(), RunId::from_u128(7)),
            namespace: NamespaceName::new("alice").unwrap(),
            origin: "script".into(),
            suffix_len: 12,
            code: Digest::of(b"script"),
            action_set: Digest::of(b"action_set"),
            status: RunStatus::Active,
            epoch: Epoch(1),
            attempt: Attempt::FIRST,
            sealed: None,
            failures: 0,
            attempts: 0,
            quarantine: None,
            waits: Vec::new(),
            expires_at: Timestamp(u64::MAX),
            history_len: 0,
            metadata: RunMetadata::default(),
            idempotency_key: None,
            created_at: Timestamp(0),
            updated_at: Timestamp(0),
            ended_at: None,
        }
    }

    /// `ask` is a harness action, `github.get` a server's.
    fn action_set() -> ActionSet {
        let server = McpServer::new(
            "github",
            McpTransport::Http {
                url: "https://mcp.example.com/github".into(),
            },
        );
        let get = Action::new(
            "github.get",
            ActionRoute::Mcp {
                server: "github".into(),
            },
            json!({}),
        );
        let ask = Action::new("ask", ActionRoute::Harness { group: None }, json!({}));
        ActionSet::new([server], [get, ask]).unwrap()
    }

    /// A long history of every kind of event: calls completed, failed and left pending; timers
    /// fired, cancelled, used as a signal's timeout, timed out, left pending; signals received
    /// and left awaited; logs; then a cancellation the code ends with.
    fn varied_history(rounds: u32) -> Vec<HistoryEvent> {
        let mut events = vec![Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            code: Digest::of(b"script"),
            action_set: Digest::of(b"action_set"),
            input: json!({}).into(),
            seed: Seed::from_bytes([1; 32]),
            epoch: Epoch(1),
            expires_at: Timestamp(u64::MAX),
            mode: RunMode::Live,
        }))];
        let callsite = |line| Some(Callsite { line, column: 1 });
        let mut next = 0;
        let mut seq = || {
            next += 1;
            Seq(next)
        };
        for round in 0..rounds {
            let (ask, get, sleep, timeout) = (seq(), seq(), seq(), seq());
            let at = 1_000 * u64::from(round);
            events.push(Event::Command(Command::ScheduleCall {
                seq: ask,
                action: "ask".into(),
                args: json!({ "round": round }).into(),
                callsite: callsite(ask.0),
            }));
            events.push(Event::Command(Command::ScheduleCall {
                seq: get,
                action: "github.get".into(),
                args: json!({ "issue": round }).into(),
                callsite: callsite(get.0),
            }));
            events.push(Event::Command(Command::StartTimer {
                seq: sleep,
                duration_ms: 500,
                fire_at: Timestamp(at + 500),
                callsite: callsite(sleep.0),
            }));
            events.push(Event::Command(Command::StartTimer {
                seq: timeout,
                duration_ms: 900,
                fire_at: Timestamp(at + 900),
                callsite: None,
            }));
            events.push(Event::Annotation(Annotation::SignalAwaited {
                name: "go".into(),
                timeout: Some(timeout),
                callsite: callsite(timeout.0),
            }));
            events.push(Event::Annotation(Annotation::Log {
                level: LogLevel::Info,
                message: format!("round {round}"),
                callsite: None,
            }));
            events.push(Event::Completion(Completion::CallCompleted {
                seq: ask,
                result: json!({ "answer": round }).into(),
            }));
            events.push(if round % 2 == 0 {
                Event::Command(Command::CancelTimer { seq: sleep })
            } else {
                Event::Completion(Completion::TimerFired { seq: sleep })
            });
            match round % 3 {
                0 => events.push(Event::Completion(Completion::TimerFired { seq: timeout })),
                1 => {
                    events.push(Event::Completion(Completion::SignalReceived {
                        name: "go".into(),
                        payload: json!({ "round": round }).into(),
                    }));
                    events.push(Event::Command(Command::CancelTimer { seq: timeout }));
                }
                // Received, its timeout left to run: a timer of its own from then on.
                _ => events.push(Event::Completion(Completion::SignalReceived {
                    name: "go".into(),
                    payload: json!(null).into(),
                })),
            }
            if round % 5 != 0 {
                events.push(Event::Completion(Completion::CallFailed {
                    seq: get,
                    error: ActionFailure {
                        kind: if round % 2 == 0 {
                            ActionFailureKind::Timeout
                        } else {
                            ActionFailureKind::Error
                        },
                        message: "no".into(),
                        data: None,
                    },
                }));
            }
            events.push(Event::Annotation(Annotation::SignalAwaited {
                name: "next".into(),
                timeout: None,
                callsite: None,
            }));
            if round % 4 != 0 {
                events.push(Event::Completion(Completion::SignalReceived {
                    name: "next".into(),
                    payload: json!(round).into(),
                }));
            }
        }
        events.push(Event::Completion(Completion::CancelRequested {
            reason: CancelReason::Requested(None),
        }));
        events.push(Event::Command(Command::FailRun {
            error: ErrorInfo {
                name: ErrorInfo::CANCELLED.into(),
                message: "cancelled".into(),
                stack: None,
            },
        }));
        events
            .into_iter()
            .enumerate()
            .map(|(index, event)| HistoryEvent {
                index: index as u64,
                recorded_at: Timestamp(10 * index as u64),
                event,
            })
            .collect()
    }

    fn hours(deltas: Vec<CounterDelta>, into: &mut BTreeMap<Hour, Counters>) {
        for delta in deltas {
            if let CounterDelta::Hour { hour, counts, .. } = delta {
                into.entry(hour).or_default().add(&counts);
            }
        }
    }

    /// What one activation derives from the events `from..to`.
    struct Derived {
        summary: IndexedSummary,
        spans: Vec<crate::ports::Span>,
        measurements: Vec<RunMeasurement>,
        shapes: Vec<CounterDelta>,
        counters: Vec<CounterDelta>,
    }

    fn derive(
        history: &[HistoryEvent],
        from: usize,
        to: usize,
        previous: Option<&IndexedSummary>,
    ) -> Derived {
        let (head, action_set) = (head(), action_set());
        let history = &history[..to];
        let before = RunState::from_history(&history[..from]).unwrap();
        let after = RunState::from_history(history).unwrap();
        let summary = summary_of(
            &head,
            "headline",
            history,
            &after,
            &action_set,
            &BTreeMap::new(),
            previous,
        );
        let openings = Openings::new(history, from, &before);
        let config = TelemetryConfig::default();
        let payloads = BTreeMap::new();
        let telemetry = RunTelemetry {
            head: &head,
            config: &config,
            payloads: &payloads,
        };
        Derived {
            spans: telemetry.spans(history, &before, from as u64, to as u64),
            measurements: telemetry.measurements(history, &before, from as u64, to as u64),
            shapes: shapes_of(&head, history, from, &openings),
            counters: counters_of(
                &head,
                history,
                from,
                &openings,
                &summary,
                Duration::ZERO,
                Timestamp(0),
            ),
            summary,
        }
    }

    #[test]
    fn an_activation_derives_from_its_new_events_what_the_whole_history_gives() {
        let history = varied_history(60);
        let whole = derive(&history, 0, history.len(), None);
        let mut whole_hours = BTreeMap::new();
        hours(whole.counters, &mut whole_hours);
        let mut sizes = 0x2545_f491_u64;
        for chunking in ["one event", "three events", "varied"] {
            let mut previous = Some(pending_summary(&head(), "headline"));
            let (mut spans, mut measurements, mut shapes) = (Vec::new(), Vec::new(), Vec::new());
            let mut chunked_hours = BTreeMap::new();
            let mut from = 0;
            while from < history.len() {
                let size = match chunking {
                    "one event" => 1,
                    "three events" => 3,
                    _ => {
                        sizes = sizes
                            .wrapping_mul(6_364_136_223_846_793_005)
                            .wrapping_add(1);
                        1 + (sizes >> 60) as usize
                    }
                };
                let to = (from + size).min(history.len());
                let derived = derive(&history, from, to, previous.as_ref());
                assert_eq!(
                    derived.summary,
                    derive(&history, from, to, None).summary,
                    "{chunking}: the summary at {to} counted from the previous one"
                );
                spans.extend(derived.spans);
                measurements.extend(derived.measurements);
                shapes.extend(derived.shapes);
                hours(derived.counters, &mut chunked_hours);
                previous = Some(derived.summary);
                from = to;
            }
            assert_eq!(previous.as_ref(), Some(&whole.summary), "{chunking}");
            assert_eq!(spans, whole.spans, "{chunking}: the spans");
            assert_eq!(
                measurements, whole.measurements,
                "{chunking}: the measurements"
            );
            assert_eq!(shapes, whole.shapes, "{chunking}: the shapes");
            assert_eq!(chunked_hours, whole_hours, "{chunking}: the counters");
        }
        assert!(whole.spans.len() > 200, "{}", whole.spans.len());
    }
}
