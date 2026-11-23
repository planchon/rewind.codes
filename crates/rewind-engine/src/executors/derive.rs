//! What an activation writes beside its entries, derived from the history it extends
//! (`archi/05-testing-and-observability.md`, "Read model", "The projector";
//! `archi/06-storage-and-scale.md`, "Quotas"): the run's head and summary, the tool tasks and
//! timers of its new commands, the counters it moves, and the projections it feeds.
//!
//! Everything here is pure: the same history gives the same records on every node, so an
//! activation run by a remote executor commits what a local one would.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rewind_core::{
    Command, Completion, Event, HistoryEntry, JsonValue, Lifecycle, Payload, RunStatus, Seq,
    Shape, Timestamp, Toolset,
};

use crate::ports::{
    CounterDelta, Counters, DueTimer, FailureCounts, Hour, IndexedSummary, LatestRun,
    PayloadRecord, Projection, QuarantineNote, RunHead, RunRecord, RunSummary, TaskPhase,
    TaskRecord, TaskRef, TaskState, TimerChange, TimerKind, ToolCounts, ToolTask, WaitKind,
};
use crate::projector;
use crate::telemetry::{RunTelemetry, SpanKey, TelemetryConfig};

/// A run's head as the old record type, for the derivations shared with the journal path
/// (spans, lifecycle events, trace contexts).
pub(crate) fn record_of(head: &RunHead) -> RunRecord {
    RunRecord {
        run: head.run.clone(),
        namespace: head.namespace.clone(),
        script: head.script,
        toolset: head.toolset,
        status: head.status,
        epoch: head.epoch,
        expires_at: head.expires_at,
        history_len: head.history_len,
        quarantine: head.quarantine.clone(),
        failures: head.failures,
        attempts: head.attempts,
        sticky_worker: None,
        metadata: head.metadata.clone(),
        idempotency_key: head.idempotency_key.clone(),
        created_at: head.created_at,
        updated_at: head.updated_at,
        ended_at: head.ended_at,
        archive: None,
    }
}

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

/// The run's summary from its whole history: its status, its waits (each call's with its
/// task's phase), its ending, its counts, the tools it called.
pub(crate) fn summary_of(
    head: &RunHead,
    headline: &str,
    history: &[HistoryEntry],
    toolset: &Toolset,
    tasks: &BTreeMap<Seq, TaskRecord>,
) -> IndexedSummary {
    let snapshot = projector::run_snapshot(history, Some(toolset));
    let mut waits = snapshot.waits;
    for wait in &mut waits {
        if matches!(wait.kind, WaitKind::Call | WaitKind::Host)
            && let Some(seq) = wait.seq
        {
            wait.task = tasks.get(&seq).map(phase_of);
        }
    }
    let wakes_at = waits.iter().filter_map(|wait| wait.until).min();
    let mut tools = BTreeSet::new();
    let mut call_errors = BTreeSet::new();
    let (mut tool_calls, mut errors) = (0, 0);
    for entry in history {
        match &entry.event {
            Event::Command(Command::ScheduleToolCall { tool, .. }) => {
                tool_calls += 1;
                tools.insert(tool.clone());
            }
            Event::Completion(Completion::ToolCallFailed { error, .. }) => {
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
    let ended_at = history
        .iter()
        .rev()
        .find(|entry| entry.event.is_terminal())
        .map(|entry| entry.recorded_at);
    let metadata = &head.metadata;
    IndexedSummary {
        summary: RunSummary {
            run: head.run.clone(),
            namespace: head.namespace.clone(),
            script: head.script,
            headline: headline.to_owned(),
            toolset: head.toolset,
            status: if history.is_empty() {
                RunStatus::Pending
            } else {
                snapshot.status
            },
            mode: snapshot.mode,
            labels: metadata.labels.clone(),
            parent: metadata.parent.map(|parent| parent.run),
            workflow: metadata.workflow.as_ref().map(|of| of.name.clone()),
            started_by: metadata.started_by.as_ref().map(|by| by.run),
            started_at,
            ended_at,
            last_activity_at: history
                .last()
                .map_or(head.updated_at, |entry| entry.recorded_at),
            tool_calls,
            errors,
            history_len: history.len() as u64,
            waits,
            wakes_at,
            ending: snapshot.ending,
            quarantine: head.quarantine.as_ref().map(QuarantineNote::from),
        },
        tools,
        call_errors,
    }
}

/// The summary of a run created and not activated yet: `pending`.
pub(crate) fn pending_summary(head: &RunHead, headline: &str) -> IndexedSummary {
    let mut summary = summary_of(
        head,
        headline,
        &[],
        &Toolset::empty(),
        &BTreeMap::new(),
    );
    summary.summary.status = RunStatus::Pending;
    summary
}

/// The run's head after an activation that left it in `status`.
pub(crate) fn head_after(
    head: &RunHead,
    history: &[HistoryEntry],
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

/// The tool tasks and timers of the new entries (`from` on): a call still pending at the end of
/// the activation, a timer still pending, the cancellation of a timer of an earlier activation,
/// and the run's deadline once `run_started` is committed. A terminal run has none: its commit
/// clears them. A task takes its route and policy from the pinned toolset; a tool the toolset
/// does not have is returned as the error.
pub(crate) fn effects_of(
    head: &RunHead,
    history: &[HistoryEntry],
    from: usize,
    state: &rewind_core::RunState,
    toolset: &Toolset,
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
            Event::Command(Command::ScheduleToolCall {
                seq, tool, args, ..
            }) if state.pending_tool_calls().contains_key(seq) => {
                let pinned = toolset.tool(tool).ok_or_else(|| tool.clone())?;
                tasks.push(TaskRecord {
                    task: ToolTask {
                        task: TaskRef {
                            run: run.clone(),
                            seq: *seq,
                        },
                        toolset: head.toolset,
                        tool: tool.clone(),
                        route: pinned.route.clone(),
                        args: args.clone(),
                        policy: pinned.policy,
                        state: if pinned.policy.approval {
                            TaskState::Approval
                        } else {
                            TaskState::Ready {
                                next_attempt_at: now,
                            }
                        },
                        attempt: 1,
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
                        kind: TimerKind::Script {
                            run: run.clone(),
                            seq: *seq,
                        },
                    }));
                }
            }
            Event::Command(Command::CancelTimer { seq }) if !started_here.contains(seq) => {
                timers.push(TimerChange::Cancel(TimerKind::Script {
                    run: run.clone(),
                    seq: *seq,
                }));
            }
            _ => {}
        }
    }
    Ok((tasks, timers))
}

/// The shapes of the results of the new entries, before their payloads leave them.
pub(crate) fn shapes_of(head: &RunHead, history: &[HistoryEntry], from: usize) -> Vec<CounterDelta> {
    let mut tools: BTreeMap<Seq, &str> = BTreeMap::new();
    let mut deltas = Vec::new();
    for (index, entry) in history.iter().enumerate() {
        match &entry.event {
            Event::Command(Command::ScheduleToolCall { seq, tool, .. }) => {
                tools.insert(*seq, tool);
            }
            Event::Completion(Completion::ToolCallCompleted {
                seq,
                result: Payload::Value(value),
            }) if index >= from => {
                if let Some(tool) = tools.get(seq) {
                    deltas.push(CounterDelta::Shape {
                        tenant: head.run.tenant.clone(),
                        namespace: head.namespace.clone(),
                        tool: (*tool).to_owned(),
                        shape: Shape::of(value),
                    });
                }
            }
            _ => {}
        }
    }
    deltas
}

/// The counters the new entries (`from` on) move, per hour of their `recorded_at`: the run's
/// start and end, its duration and failure group, its workflow and script, the calls of each
/// tool; and the CPU time of the activation in the hour of `now`.
pub(crate) fn counters_of(
    head: &RunHead,
    history: &[HistoryEntry],
    from: usize,
    summary: &IndexedSummary,
    cpu: Duration,
    now: Timestamp,
) -> Vec<CounterDelta> {
    let mut hours: BTreeMap<Hour, Counters> = BTreeMap::new();
    let workflow = head.metadata.workflow.as_ref().map(|of| of.name.clone());
    let mut scheduled: BTreeMap<Seq, (Timestamp, &str)> = BTreeMap::new();
    let started_at = summary.summary.started_at;
    for (index, entry) in history.iter().enumerate() {
        let new = index >= from;
        match &entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(_)) if new => {
                let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
                counters.runs.started += 1;
                if let Some(workflow) = &workflow {
                    counters.workflows.entry(workflow.clone()).or_default().started += 1;
                }
                let script = counters.scripts.entry(head.script).or_default();
                script.runs.started += 1;
                script.first_started_at = Some(entry.recorded_at);
                script.last_started_at = Some(entry.recorded_at);
            }
            Event::Command(Command::ScheduleToolCall { seq, tool, .. }) => {
                scheduled.insert(*seq, (entry.recorded_at, tool));
                if new {
                    let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
                    counters.tools.entry(tool.clone()).or_default().calls += 1;
                    counters
                        .scripts
                        .entry(head.script)
                        .or_default()
                        .tools
                        .insert(tool.clone());
                }
            }
            Event::Completion(Completion::ToolCallCompleted { seq, .. }) if new => {
                if let Some((at, tool)) = scheduled.get(seq) {
                    let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
                    let calls = counters.tools.entry((*tool).to_owned()).or_default();
                    calls.completed += 1;
                    calls.latency.record(entry.recorded_at.0.saturating_sub(at.0));
                }
            }
            Event::Completion(Completion::ToolCallFailed { seq, error }) if new => {
                if let Some((at, tool)) = scheduled.get(seq) {
                    let counters = hours.entry(Hour::of(entry.recorded_at)).or_default();
                    let calls = counters.tools.entry((*tool).to_owned()).or_default();
                    calls.failed += 1;
                    *calls.errors.entry(error.kind).or_default() += 1;
                    calls.latency.record(entry.recorded_at.0.saturating_sub(at.0));
                }
            }
            _ => {}
        }
        if new && entry.event.is_terminal() {
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
                .entry(head.script)
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
                        scripts: BTreeMap::from([(head.script, 1)]),
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

/// The retries of a call that ended after its first attempt.
pub(crate) fn retries_counters(
    head: &RunHead,
    tool: &str,
    attempt: u32,
    now: Timestamp,
) -> Option<CounterDelta> {
    let retries = u64::from(attempt.checked_sub(1)?);
    if retries == 0 {
        return None;
    }
    Some(CounterDelta::Hour {
        tenant: head.run.tenant.clone(),
        namespace: head.namespace.clone(),
        hour: Hour::of(now),
        counts: Counters {
            tools: BTreeMap::from([(
                tool.to_owned(),
                ToolCounts {
                    retries,
                    ..ToolCounts::default()
                },
            )]),
            ..Counters::default()
        },
    })
}

/// What the new entries (`from` on) feed the projections: the spans and logs that end in them,
/// the log records named by the activation span `H(run, "activation", from)`, and the lifecycle
/// webhooks.
pub(crate) fn projections_of(
    head: &RunHead,
    history: &[HistoryEntry],
    from: usize,
    toolset: &Toolset,
    payloads: &BTreeMap<rewind_core::Digest, PayloadRecord>,
    telemetry: &TelemetryConfig,
) -> Vec<Projection> {
    if history.len() <= from {
        return Vec::new();
    }
    let record = record_of(head);
    let run_telemetry = RunTelemetry {
        record: &record,
        config: telemetry,
        payloads,
    };
    let (start, end) = (from as u64, history.len() as u64);
    let mut projections = Vec::new();
    if run_telemetry.exported(history) {
        let spans = run_telemetry.spans(history, start, end);
        let logs = run_telemetry.logs(history, start, end, SpanKey::ActivationAt(start));
        if !spans.is_empty() || !logs.is_empty() {
            projections.push(Projection::Otel { spans, logs });
        }
    }
    for event in projector::lifecycle_events(&record, Some(toolset), history, from) {
        projections.push(Projection::Webhooks(event));
    }
    projections
}

/// The runs a history started with a workflow's `start` and whose end it does not hold, by
/// call, and whether the start was attached (not `detached`).
pub(crate) fn children_of(history: &[HistoryEntry]) -> Vec<(Seq, rewind_core::RunId, bool)> {
    let mut detached: BTreeMap<Seq, bool> = BTreeMap::new();
    for entry in history {
        if let Event::Command(Command::ScheduleToolCall {
            seq, tool, args, ..
        }) = &entry.event
            && matches!(
                rewind_core::WorkflowOp::of_tool(tool),
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
        if matches!(wait.kind, WaitKind::Call | WaitKind::Host)
            && let Some(seq) = wait.seq
        {
            wait.task = phases.get(&seq).copied();
        }
    }
    summary
}
