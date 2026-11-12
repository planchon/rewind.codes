//! The engine's state as the HTTP API `/v1` shows it (`ptc-api`'s DTOs), so that the agent
//! tools render exactly what the SDK's agent tools render from the same DTOs.

use ptc_api::common::{
    AbortReason, Callsite, Labels, LogLevel, PAYLOAD_VALUE_LIMIT, Payload, ToolFailure, timestamp,
};
use ptc_api::runs::{
    FailedCall, ForkParent, LogLine, Quarantine, QuarantineCause, Run, RunError, RunSummary,
    Waiting,
};
use ptc_api::scripts::{CheckResult, Diagnostic, Location, Severity};
use ptc_api::tasks::{ParkedReason, TaskState};
use ptc_core::{
    Annotation, Command, Completion, Event, HistoryEntry, Lifecycle, RunOutcome, RunState, Seq,
};
use ptc_engine::EngineError;
use ptc_engine::ports::{JournalStore, ParkKind, QuarantineKind, RunRef};

use crate::world::World;

/// The last log lines a run's state carries.
const LOG_LINES: usize = 20;

/// `GET /v1/runs/{id}`.
pub async fn run(world: &World, run: &RunRef) -> Result<Run, EngineError> {
    let engine = &world.engine;
    let record = engine
        .get_run(run)
        .await?
        .ok_or(EngineError::RunNotFound(run.id))?;
    let history = engine.export_history(run).await?;
    let state = RunState::from_history(&history).map_err(|error| EngineError::CorruptHistory {
        run_id: run.id,
        error,
    })?;
    let source = engine
        .journal()
        .get_script(&run.tenant, record.script)
        .await?
        .map(|stored| stored.script.source().to_owned())
        .unwrap_or_default();
    let tasks = engine.tool_tasks(run).await?;

    let input = history.iter().find_map(|entry| match &entry.event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => started.input.value().cloned(),
        _ => None,
    });
    let (output, error, abort) = match state.outcome() {
        Some(RunOutcome::Completed { output }) => (
            output
                .value()
                .map(|value| Payload::of(value.clone(), PAYLOAD_VALUE_LIMIT)),
            None,
            None,
        ),
        Some(RunOutcome::Failed { error }) => {
            (None, Some(run_error(error, &history, &source)), None)
        }
        Some(RunOutcome::Aborted { reason }) => (None, None, Some(AbortReason::from(reason))),
        None => (None, None, None),
    };

    let mut waiting_for = Vec::new();
    if !state.is_terminal() {
        let timeouts: Vec<Seq> = state
            .awaited_signals()
            .iter()
            .filter_map(|awaited| awaited.timeout)
            .collect();
        for (seq, tool) in state.pending_tool_calls() {
            let task = tasks.iter().find(|task| task.task.seq == *seq);
            let (task_state, parked) = match task.map(|task| &task.state) {
                Some(ptc_engine::ports::TaskState::Approval) => (TaskState::Approval, None),
                Some(ptc_engine::ports::TaskState::Ready { .. }) => (TaskState::Ready, None),
                Some(ptc_engine::ports::TaskState::Claimed { .. }) => (TaskState::Claimed, None),
                Some(ptc_engine::ports::TaskState::Deferred { .. }) => (TaskState::Deferred, None),
                Some(ptc_engine::ports::TaskState::Parked(reason)) => (
                    TaskState::Parked,
                    Some(match reason.kind {
                        ParkKind::SchemaChanged => ParkedReason::SchemaChanged,
                        ParkKind::CredentialsExpired => ParkedReason::CredentialsExpired,
                    }),
                ),
                None => (TaskState::Done, None),
            };
            waiting_for.push(Waiting::ToolCall {
                seq: seq.0,
                tool: tool.clone(),
                task_id: format!("{}/{}", run.id, seq.0),
                state: task_state,
                parked,
                attempt: task.map_or(1, |task| task.attempt.max(1)),
                callsite: command_callsite(&history, *seq),
            });
        }
        for (seq, fire_at) in state.pending_timers() {
            if timeouts.contains(seq) {
                continue;
            }
            waiting_for.push(Waiting::Timer {
                seq: seq.0,
                fire_at: timestamp(*fire_at),
                callsite: command_callsite(&history, *seq),
            });
        }
        for awaited in state.awaited_signals() {
            let callsite = history
                .iter()
                .find(|entry| entry.index == awaited.since)
                .and_then(|entry| match &entry.event {
                    Event::Annotation(Annotation::SignalAwaited { callsite, .. }) => *callsite,
                    _ => None,
                });
            waiting_for.push(Waiting::Signal {
                name: awaited.name.clone(),
                timeout_at: awaited
                    .timeout
                    .and_then(|seq| state.pending_timers().get(&seq))
                    .map(|at| timestamp(*at)),
                callsite: callsite.map(Callsite::from),
            });
        }
    }

    let logs: Vec<LogLine> = history
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Annotation(Annotation::Log {
                level,
                message,
                callsite,
            }) => Some(LogLine {
                idx: entry.index,
                at: timestamp(entry.recorded_at),
                level: LogLevel::from(*level),
                message: message.clone(),
                callsite: callsite.map(Callsite::from),
            }),
            _ => None,
        })
        .collect();
    let logs = logs[logs.len().saturating_sub(LOG_LINES)..].to_vec();

    let labels: Labels = record.metadata.labels.clone().into_iter().collect();
    Ok(Run {
        id: run.id.to_string(),
        status: record.status.into(),
        quarantine: record.quarantine.as_ref().map(|quarantine| Quarantine {
            cause: match quarantine.kind {
                QuarantineKind::NonDeterminism => QuarantineCause::NonDeterminism,
                QuarantineKind::Failures => QuarantineCause::RepeatedFailures,
            },
            reason: quarantine.reason.clone(),
        }),
        script: record.script.to_hex(),
        toolset: record.toolset.to_hex(),
        epoch: record.epoch.0,
        mode: state
            .mode()
            .map_or(ptc_api::common::RunMode::Live, Into::into),
        labels,
        trace_id: run.id.to_string().replace('-', ""),
        parent: record.metadata.parent.map(|parent| ForkParent {
            run: parent.run.to_string(),
            at: parent.at,
        }),
        input: Payload::of(input.unwrap_or_default(), PAYLOAD_VALUE_LIMIT),
        output,
        error,
        abort,
        waiting_for,
        logs,
        history_len: record.history_len,
        created_at: timestamp(record.created_at),
        updated_at: timestamp(record.updated_at),
        ended_at: record.ended_at.map(timestamp),
        expires_at: timestamp(record.expires_at),
    })
}

/// A run in `GET /v1/runs`.
pub async fn summary(world: &World, run: &RunRef) -> Result<RunSummary, EngineError> {
    let record = world
        .engine
        .get_run(run)
        .await?
        .ok_or(EngineError::RunNotFound(run.id))?;
    let history = world.engine.history(run).await?;
    let state = RunState::from_history(&history).ok();
    let mut tool_calls = 0;
    let mut errors = 0;
    for entry in &history {
        match &entry.event {
            Event::Command(Command::ScheduleToolCall { .. }) => tool_calls += 1,
            Event::Completion(Completion::ToolCallFailed { .. }) => errors += 1,
            _ => {}
        }
    }
    let error = match state.as_ref().and_then(RunState::outcome) {
        Some(RunOutcome::Failed { error }) => Some(error.name.clone()),
        Some(RunOutcome::Aborted { reason }) => Some(reason.kind.as_str().to_owned()),
        _ => None,
    };
    Ok(RunSummary {
        id: run.id.to_string(),
        status: record.status.into(),
        quarantined: record.quarantine.is_some(),
        script: record.script.to_hex(),
        toolset: record.toolset.to_hex(),
        labels: record.metadata.labels.clone().into_iter().collect(),
        started_at: timestamp(record.created_at),
        ended_at: record.ended_at.map(timestamp),
        last_activity_at: timestamp(record.updated_at),
        tool_calls,
        errors,
        history_len: record.history_len,
        error,
        parent: record.metadata.parent.map(|parent| parent.run.to_string()),
    })
}

/// The `schedule_tool_call` or `start_timer` of `seq`: where the script made it.
fn command_callsite(history: &[HistoryEntry], seq: Seq) -> Option<Callsite> {
    history.iter().find_map(|entry| match &entry.event {
        Event::Command(Command::ScheduleToolCall {
            seq: s, callsite, ..
        })
        | Event::Command(Command::StartTimer {
            seq: s, callsite, ..
        }) if *s == seq => callsite.map(Callsite::from),
        _ => None,
    })
}

/// How the script failed, with where: the innermost frame of its stack in `script.ts`, or the
/// call whose `ToolError` it did not catch.
fn run_error(error: &ptc_core::ScriptError, history: &[HistoryEntry], source: &str) -> RunError {
    let failed_call = (error.name == "ToolError")
        .then(|| failed_call(history))
        .flatten();
    let location = error
        .stack
        .as_deref()
        .and_then(stack_location)
        .or_else(|| failed_call.as_ref().and_then(|call| call.callsite));
    RunError {
        name: error.name.clone(),
        message: error.message.clone(),
        stack: error.stack.clone(),
        frame: location.and_then(|location| frame(source, location.line, location.column)),
        location,
        failed_call,
    }
}

/// The last call that failed before the run did: the one whose `ToolError` ended it.
fn failed_call(history: &[HistoryEntry]) -> Option<FailedCall> {
    let (seq, failure) = history.iter().rev().find_map(|entry| match &entry.event {
        Event::Completion(Completion::ToolCallFailed { seq, error }) => Some((*seq, error)),
        _ => None,
    })?;
    history.iter().find_map(|entry| match &entry.event {
        Event::Command(Command::ScheduleToolCall {
            seq: s,
            tool,
            args,
            callsite,
        }) if *s == seq => Some(FailedCall {
            seq: seq.0,
            tool: tool.clone(),
            args: Payload::of(
                args.value().cloned().unwrap_or_default(),
                PAYLOAD_VALUE_LIMIT,
            ),
            error: ToolFailure::from(failure),
            callsite: callsite.map(Callsite::from),
        }),
        _ => None,
    })
}

/// `at main (file:///script.ts:5:31)` → 5:31.
fn stack_location(stack: &str) -> Option<Callsite> {
    let (_, rest) = stack.split_once("script.ts:")?;
    let mut parts = rest.split(|c: char| !c.is_ascii_digit());
    let line = parts.next()?.parse().ok()?;
    let column = parts.next()?.parse().ok()?;
    Some(Callsite { line, column })
}

/// A few lines of the source around a position, the position marked, in the format of the
/// checker's diagnostics.
pub fn frame(source: &str, line: u32, column: u32) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let index = usize::try_from(line).ok()?.checked_sub(1)?;
    let current = lines.get(index)?;
    let before = index
        .checked_sub(1)
        .filter(|previous| !lines[*previous].trim().is_empty());
    let after =
        Some(index + 1).filter(|next| lines.get(*next).is_some_and(|l| !l.trim().is_empty()));
    let width = (after.unwrap_or(index) + 1).to_string().len();
    let numbered =
        |n: usize, text: &str| format!("{:>width$} | {text}", n + 1).trim_end().to_owned();
    let mut frame = Vec::new();
    if let Some(previous) = before {
        frame.push(numbered(previous, lines[previous]));
    }
    frame.push(numbered(index, current));
    let column = usize::try_from(column).ok()?.saturating_sub(1);
    let padding: String = current
        .chars()
        .take(column)
        .map(|c| if c == '\t' { '\t' } else { ' ' })
        .collect();
    frame.push(format!("{:width$} | {padding}^", ""));
    if let Some(next) = after {
        frame.push(numbered(next, lines[next]));
    }
    Some(frame.join("\n"))
}

/// `POST /v1/check`, from the checker's diagnostics.
pub fn check_result(diagnostics: &[ptc_engine::ports::Diagnostic]) -> CheckResult {
    let diagnostics: Vec<Diagnostic> = diagnostics.iter().map(diagnostic).collect();
    CheckResult {
        ok: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error),
        diagnostics,
    }
}

pub fn diagnostic(diagnostic: &ptc_engine::ports::Diagnostic) -> Diagnostic {
    Diagnostic {
        severity: match diagnostic.severity {
            ptc_engine::ports::Severity::Error => Severity::Error,
            ptc_engine::ports::Severity::Warning => Severity::Warning,
        },
        code: match &diagnostic.source {
            ptc_engine::ports::DiagnosticSource::TypeScript { code } => format!("TS{code}"),
            ptc_engine::ports::DiagnosticSource::Rule(rule) => rule.clone(),
        },
        message: diagnostic.message.clone(),
        location: Some(Location {
            line: diagnostic.line,
            column: diagnostic.column,
            end_line: None,
            end_column: None,
        }),
        frame: (!diagnostic.frame.is_empty()).then(|| diagnostic.frame.clone()),
        hint: diagnostic.hint.clone(),
    }
}

/// A script refused at start by the sandbox's checks (imports, names, default export): one
/// diagnostic at the position its message names.
pub fn start_diagnostic(message: &str, source: &str) -> Diagnostic {
    let location = stack_location(message);
    Diagnostic {
        severity: Severity::Error,
        code: "invalid-script".into(),
        message: message.to_owned(),
        location: location.map(|at| Location {
            line: at.line,
            column: at.column,
            end_line: None,
            end_column: None,
        }),
        frame: location.and_then(|at| frame(source, at.line, at.column)),
        hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stacks_point_at_the_script() {
        assert_eq!(
            stack_location("RangeError: invalid amount -5\n    at main (file:///script.ts:5:31)"),
            Some(Callsite {
                line: 5,
                column: 31
            })
        );
        assert_eq!(stack_location("ToolError: crm.get_order failed"), None);
    }

    #[test]
    fn frames_mark_the_column_between_their_neighbors() {
        let source = "import { crm } from \"ptc:tools\";\n\nexport default async function main() {\n  const order = await crm.get_order({ id: \"O-504\" });\n  return order;\n}\n";
        assert_eq!(
            frame(source, 4, 23).unwrap(),
            "3 | export default async function main() {\n4 |   const order = await crm.get_order({ id: \"O-504\" });\n  |                       ^\n5 |   return order;"
        );
        assert_eq!(frame(source, 40, 1), None);
    }
}
