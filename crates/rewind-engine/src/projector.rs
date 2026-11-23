//! What a run's committed entries project (`archi/05-testing-and-observability.md`, "The
//! projector", "Read model"): its summary in the run index, and its lifecycle events.
//!
//! An activation derives them from the run's history as the batch that commits its entries
//! does (`executors::derive`): the summary is written in the same batch, and what leaves the
//! partition (spans and logs, lifecycle webhooks) goes through its outbox
//! (`archi/06-storage-and-scale.md`, "The outbox").
//!
//! A run's snapshot is folded from its history ([`RunState`]):
//!
//! - a signal awaited with an empty mailbox is a `signal` wait since its `signal_awaited`, until
//!   its timeout timer's due time; that timer is not a wait of its own;
//! - a pending timer is a `timer` wait since its `start_timer`, until its due time;
//! - a pending call is a `host` wait when the pinned toolset routes its tool to the host, a
//!   `call` wait otherwise, since its `schedule_tool_call`;
//! - waits are ordered by `since`, then kind, then seq and name;
//! - a failure or an abort gives the run's [`Ending`](crate::ports::Ending), its signature and
//!   fingerprint ([`failures`]).
//!
//! Lifecycle events carry a deterministic id for receivers to deduplicate: `{run}/{index}` of
//! the entry they are projected from.

use std::collections::BTreeSet;

use rewind_core::{
    Annotation, Callsite, Command, Event, HistoryEntry, Lifecycle, RunState, RunStatus,
    ScriptError, Seq, Timestamp, ToolRoute, Toolset,
};

use crate::failures;
use crate::ports::{
    RunHead, RunSnapshot, TaskPhase, TaskProgress, TaskState, ToolTask, Wait, WaitKind,
    WebhookEvent, WebhookEventKind,
};

/// The lifecycle events of the entries from `from` to the end of `history`, which leave the
/// run in `state`: the run's end (`run.completed`, `run.failed`), or, when the range leaves the
/// run waiting for the outside world only (signals, host tools), `run.suspended`. Each is named
/// by the index of the entry it is projected from.
pub(crate) fn lifecycle_events(
    head: &RunHead,
    toolset: Option<&Toolset>,
    history: &[HistoryEntry],
    from: usize,
    state: &RunState,
) -> Vec<WebhookEvent> {
    let event = |index: u64, occurred_at: Timestamp, kind: WebhookEventKind| WebhookEvent {
        id: format!("{}/{index}", head.run.id),
        tenant: head.run.tenant.clone(),
        namespace: Some(head.namespace.clone()),
        run: Some(head.run.id),
        labels: head.metadata.labels.clone(),
        occurred_at,
        kind,
    };
    let Some(last) = history.last() else {
        return Vec::new();
    };
    if history.len() <= from {
        return Vec::new();
    }
    let ended = history[from..].iter().find_map(|entry| {
        let kind = match &entry.event {
            Event::Command(Command::CompleteRun { output }) => WebhookEventKind::RunCompleted {
                output: output.clone(),
            },
            Event::Command(Command::FailRun { error }) => WebhookEventKind::RunFailed {
                status: if error.name == ScriptError::CANCELLED {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                },
                error: format!("{}: {}", error.name, error.message),
            },
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => WebhookEventKind::RunFailed {
                status: RunStatus::Aborted,
                error: reason.to_string(),
            },
            _ => return None,
        };
        Some(event(entry.index, entry.recorded_at, kind))
    });
    if let Some(ended) = ended {
        return vec![ended];
    }
    let waiting_for = waiting_for(state, toolset);
    if waiting_for.is_empty() {
        return Vec::new();
    }
    vec![event(
        last.index,
        last.recorded_at,
        WebhookEventKind::RunSuspended { waiting_for },
    )]
}

/// What a run that has not ended waits for from the outside world: signals and host tools.
/// Empty when it is still running something of its own: a call Rewind dispatches.
fn waiting_for(state: &RunState, toolset: Option<&Toolset>) -> Vec<String> {
    let route = |tool: &str| {
        toolset
            .and_then(|toolset| toolset.tool(tool))
            .map(|t| &t.route)
    };
    let mut waiting: Vec<String> = state
        .awaited_signals()
        .iter()
        .map(|wait| format!("signal {:?}", wait.name))
        .collect();
    for (seq, tool) in state.pending_tool_calls() {
        match route(tool) {
            Some(ToolRoute::Host { .. }) => waiting.push(format!("tool {tool} {seq}")),
            _ => return Vec::new(),
        }
    }
    waiting
}

/// A task's state as the read model keeps it: its seq, its attempt and its phase.
pub fn task_progress(task: &ToolTask) -> TaskProgress {
    TaskProgress {
        seq: task.task.seq,
        attempt: task.attempt,
        phase: match task.state {
            TaskState::Approval => TaskPhase::Approval,
            TaskState::Ready { .. } => TaskPhase::Ready,
            TaskState::Claimed { .. } => TaskPhase::Claimed,
            TaskState::Deferred { .. } => TaskPhase::Deferred,
            TaskState::Parked(_) => TaskPhase::Parked,
        },
    }
}

/// The first line of a script that is not blank, trimmed: what free text searches in a script.
pub fn headline(source: &str) -> String {
    source
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_owned()
}

/// The snapshot of a run, folded from its history (see the [module](self)): its status and
/// mode, what it waits for while active, and how it ended. The pinned toolset tells the calls
/// of host tools from the calls Rewind sends; without it, every call is a `call`. A history that
/// cannot be folded past an entry gives the snapshot of the entries before it.
pub fn run_snapshot(history: &[HistoryEntry], toolset: Option<&Toolset>) -> RunSnapshot {
    let mut state = RunState::new();
    for entry in history {
        if let Err(error) = state.apply(entry) {
            tracing::warn!(%error, "a run's history cannot be folded: its snapshot stops before");
            break;
        }
    }
    let folded = &history[..usize::try_from(state.history_len())
        .unwrap_or(usize::MAX)
        .min(history.len())];
    let waits = if state.status() == RunStatus::Active {
        waits(folded, &state, toolset)
    } else {
        Vec::new()
    };
    RunSnapshot {
        history_len: state.history_len(),
        status: state.status(),
        mode: state.mode(),
        waits,
        ending: state.outcome().and_then(failures::ending),
    }
}

/// The entry that opened the call or timer `seq`, pending in `state`, read at its index in
/// `history`.
pub(crate) fn opening<'h>(
    history: &'h [HistoryEntry],
    state: &RunState,
    seq: Seq,
) -> Option<&'h HistoryEntry> {
    let index = state.opened_by(seq)?;
    history
        .get(usize::try_from(index).ok()?)
        .filter(|entry| entry.index == index)
}

/// The waits of an active run, by `since`, then kind, then seq and name: what `state` holds
/// pending, each read at the entry that opened it.
pub(crate) fn waits(
    history: &[HistoryEntry],
    state: &RunState,
    toolset: Option<&Toolset>,
) -> Vec<Wait> {
    // The entry that opened a timer or a call: when, and where in the script.
    let opened = |seq: &Seq| -> Option<(Timestamp, Option<Callsite>)> {
        let entry = opening(history, state, *seq)?;
        match &entry.event {
            Event::Command(
                Command::ScheduleToolCall { callsite, .. } | Command::StartTimer { callsite, .. },
            ) => Some((entry.recorded_at, *callsite)),
            _ => None,
        }
    };
    let timers = state.pending_timers();
    let timeouts: BTreeSet<Seq> = state
        .awaited_signals()
        .iter()
        .filter_map(|signal| signal.timeout)
        .collect();
    let mut waits = Vec::new();
    for signal in state.awaited_signals() {
        let Some(entry) = usize::try_from(signal.since)
            .ok()
            .and_then(|index| history.get(index))
        else {
            continue;
        };
        let callsite = match &entry.event {
            Event::Annotation(Annotation::SignalAwaited { callsite, .. }) => *callsite,
            _ => None,
        };
        waits.push(Wait {
            kind: WaitKind::Signal,
            seq: signal.timeout,
            name: Some(signal.name.clone()),
            since: entry.recorded_at,
            until: signal.timeout.and_then(|timer| timers.get(&timer)).copied(),
            callsite,
            task: None,
        });
    }
    for (seq, fire_at) in timers {
        if timeouts.contains(seq) {
            continue;
        }
        let Some((since, callsite)) = opened(seq) else {
            continue;
        };
        waits.push(Wait {
            kind: WaitKind::Timer,
            seq: Some(*seq),
            name: None,
            since,
            until: Some(*fire_at),
            callsite,
            task: None,
        });
    }
    for (seq, tool) in state.pending_tool_calls() {
        let Some((since, callsite)) = opened(seq) else {
            continue;
        };
        let host = toolset
            .and_then(|toolset| toolset.tool(tool))
            .is_some_and(|tool| matches!(tool.route, ToolRoute::Host { .. }));
        waits.push(Wait {
            kind: if host { WaitKind::Host } else { WaitKind::Call },
            seq: Some(*seq),
            name: Some(tool.clone()),
            since,
            until: None,
            callsite,
            task: None,
        });
    }
    waits.sort_by(|a, b| (a.since, a.kind, a.seq, &a.name).cmp(&(b.since, b.kind, b.seq, &b.name)));
    waits
}
