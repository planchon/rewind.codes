//! `rewind replay <run|run.json> [--code fixed.ts [--at n]]`, in this process
//! (`archi/05-testing-and-observability.md`, "Replay, debugging and forks").
//!
//! The run export is read from the file, or exported by the server
//! (`GET /v1/runs/{id}/export`); either way the replay runs here, on a fresh V8 executor,
//! and calls no action. Without `--code`, the run's own code must reproduce the history
//! exactly: a divergence is non-determinism. With `--code`, the corrected code is followed
//! against the history as a fork would follow it (`rewind_engine::fork::compatibility`): the
//! report says where it stops following the run, and which calls a fork would reuse, call
//! again, or fail with `outcome_unknown`.

use std::path::Path;

use rewind_api::runs::ExportAs;
use rewind_core::{Code, Command, Completion, Event, HistoryEvent, NonDeterminism, RunExport, Seq};
use rewind_engine::ports::{CodeExecutor, RunRef, TenantId};
use rewind_engine::replay::{self, ReplayEnd, ReplayError, ReplayInput};
use rewind_engine::{Compatibility, ForkError, ForkPoint, Limits};
use rewind_runtime::{SandboxLimits, V8Executor};
use serde::Serialize;

use super::Server;
use crate::args::ReplayArgs;
use crate::error::CliError;
use crate::exit::Exit;
use crate::input;
use crate::output::Console;

/// What `rewind replay` found, as `--json` prints it.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReplayReport {
    pub(crate) run: String,
    pub(crate) events: u64,
    /// The code reproduces the history: its own code, or the corrected one up to the end
    /// of the run (or its cancellation or abort).
    pub(crate) matches: bool,
    /// What the replay found, in one sentence.
    pub(crate) verdict: String,
    /// With `--code`: what a fork with the corrected code would do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fork: Option<ForkReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkReport {
    /// The calls whose recorded outcome the fork reuses: they are not called again.
    pub(crate) reused: Vec<CallRef>,
    /// Calls the run left without an outcome, of idempotent actions: called again with the run's
    /// key.
    pub(crate) called_again: Vec<CallRef>,
    /// Calls the run left without an outcome, of actions that are not idempotent: they fail with
    /// `outcome_unknown`.
    pub(crate) outcome_unknown: Vec<CallRef>,
    /// The fork's status once it has followed the run.
    pub(crate) status: rewind_api::common::RunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CallRef {
    pub(crate) seq: u32,
    pub(crate) action: String,
}

pub(super) async fn replay(
    server: &Server<'_>,
    args: ReplayArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let path = Path::new(&args.run);
    // A run is named by its id, a UUID, or its handle, which holds `/`: what looks like a path
    // and is no handle is a file, which must exist.
    let handle = args.run.parse::<rewind_core::RunHandle>().is_ok();
    let is_file = path.is_file()
        || (!handle
            && (args.run.contains(std::path::MAIN_SEPARATOR)
                || path
                    .extension()
                    .is_some_and(|extension| extension == "json")));
    let export = if is_file {
        let text = input::read_text(path)?;
        rewind_format::decode_run_export(&text)
            .map_err(|error| CliError::usage(format!("{}: {error}", path.display())))?
    } else {
        let client = server.client().await?;
        let document = client.export_run(&args.run, ExportAs::Run).await?;
        rewind_format::decode_run_export(&document.to_string()).map_err(|error| {
            CliError::Api(rewind_api::Error::Decode(format!(
                "the run export of run {}: {error}",
                args.run
            )))
        })?
    };
    let report = match &args.code {
        None => replay_export(&export).await?,
        Some(path) => {
            let code = Code::new(input::read_text(path)?);
            compatibility(&export, &code, args.at).await?
        }
    };
    if console.json() {
        console.print_json(&report)?;
    } else {
        console.line(&report.verdict);
        if let Some(fork) = &report.fork {
            let calls = |calls: &[CallRef]| {
                calls
                    .iter()
                    .map(|call| format!("#{} {}", call.seq, call.action))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            if !fork.reused.is_empty() {
                console.line(format!("a fork reuses: {}", calls(&fork.reused)));
            }
            if !fork.called_again.is_empty() {
                console.line(format!(
                    "a fork calls again, with the run's idempotency keys: {}",
                    calls(&fork.called_again)
                ));
            }
            if !fork.outcome_unknown.is_empty() {
                console.line(format!(
                    "in a fork, these fail with outcome_unknown (not idempotent): {}",
                    calls(&fork.outcome_unknown)
                ));
            }
            console.line(format!(
                "the fork is {} once it has followed the run",
                fork.status
            ));
        }
    }
    Ok(if report.matches {
        Exit::Success
    } else {
        Exit::Failed
    })
}

fn executor() -> Result<V8Executor, CliError> {
    V8Executor::new(SandboxLimits::default()).map_err(|error| CliError::internal(error.to_string()))
}

/// Replays a history with its own code.
pub(crate) async fn replay_export(export: &RunExport) -> Result<ReplayReport, CliError> {
    let events = export.events.len() as u64;
    let mut report = ReplayReport {
        run: export.run.to_string(),
        events,
        matches: false,
        verdict: String::new(),
        fork: None,
    };
    if let Err(error) = export.check() {
        report.verdict = format!("the history is inconsistent: {error}");
        return Ok(report);
    }
    let executor = executor()?;
    let compiled = executor.compile(&export.code, &export.action_set);
    let run = RunRef::new(TenantId::local(), export.run);
    let input = ReplayInput {
        run: &run,
        compiled: compiled.as_ref(),
        action_set: &export.action_set,
        history: &export.events,
    };
    match replay::replay(&executor, &replay::InExport, input).await {
        Ok(rebuilt) => {
            report.matches = true;
            report.verdict = match rebuilt.end {
                ReplayEnd::EndOfHistory => format!(
                    "the code replays the {events} events of the history of run {}",
                    export.run
                ),
                ReplayEnd::Aborted(reason) => format!(
                    "the code replays the {events} events of the history of run {}, up to its abort ({reason})",
                    export.run
                ),
            };
        }
        Err(ReplayError::NonDeterminism(mismatch)) => {
            report.verdict = format!("the replay diverges from the history: {mismatch}");
        }
        Err(error) => {
            report.verdict = format!("the replay fails: {error}");
        }
    }
    Ok(report)
}

/// Follows a history with a corrected code, as a fork would.
pub(crate) async fn compatibility(
    export: &RunExport,
    code: &Code,
    at: Option<u64>,
) -> Result<ReplayReport, CliError> {
    let events = export.events.len() as u64;
    let mut report = ReplayReport {
        run: export.run.to_string(),
        events,
        matches: false,
        verdict: String::new(),
        fork: None,
    };
    if let Err(error) = export.check() {
        report.verdict = format!("the history is inconsistent: {error}");
        return Ok(report);
    }
    let executor = executor()?;
    let found: Compatibility =
        match rewind_engine::fork::compatibility(&executor, export, code, at, &Limits::default())
            .await
        {
            Ok(found) => found,
            Err(ForkError::Start(refused)) => {
                return Err(CliError::CodeRefused {
                    message: format!("the corrected code cannot start: {refused}"),
                    diagnostics: Vec::new(),
                });
            }
            Err(error) => {
                return Err(CliError::internal(format!(
                    "the history cannot be followed: {error}"
                )));
            }
        };
    report.matches = found.is_compatible();
    report.verdict = match &found.point {
        ForkPoint::Diverged(mismatch) => {
            format!("the code diverges from the history: {mismatch}")
        }
        ForkPoint::At(index) => format!(
            "the code follows the history of run {} up to event {index}, where a fork goes live",
            export.run
        ),
        ForkPoint::Stopped => {
            "the code could not go on while it followed the history: a fork is aborted".to_owned()
        }
        point @ (ForkPoint::EndOfParent
        | ForkPoint::ParentCancelled(_)
        | ForkPoint::ParentAborted(_)) => format!(
            "the code is compatible with the history of run {} ({events} events): {point}",
            export.run
        ),
    };
    let actions = actions_by_seq(&export.events);
    // What a fork reuses: the calls it copies with their outcomes while it follows the run,
    // and the calls it inherits still pending, whose recorded outcomes it delivers.
    let mut reused = completed_before(&export.events, live_from(&found.point, events));
    reused.extend(found.reused.iter().copied());
    reused.sort();
    reused.dedup();
    let calls = |seqs: &[Seq]| {
        seqs.iter()
            .map(|seq| CallRef {
                seq: seq.0,
                action: actions
                    .iter()
                    .find(|(called, _)| called == seq)
                    .map(|(_, action)| action.clone())
                    .unwrap_or_default(),
            })
            .collect()
    };
    report.fork = Some(ForkReport {
        reused: calls(&reused),
        called_again: calls(&found.redispatched),
        outcome_unknown: calls(&found.unknown),
        status: found.status.into(),
    });
    Ok(report)
}

/// The event where a fork stops following the run.
fn live_from(point: &ForkPoint, events: u64) -> u64 {
    match point {
        ForkPoint::EndOfParent => events,
        ForkPoint::At(index)
        | ForkPoint::ParentCancelled(index)
        | ForkPoint::ParentAborted(index) => *index,
        ForkPoint::Diverged(
            NonDeterminism::CommandMismatch { index, .. }
            | NonDeterminism::MissingCommand { index, .. },
        ) => *index,
        ForkPoint::Diverged(NonDeterminism::UnexpectedCommand { at, .. }) => *at,
        ForkPoint::Stopped => 0,
    }
}

/// The calls scheduled and completed before event `end`.
fn completed_before(events: &[HistoryEvent], end: u64) -> Vec<Seq> {
    let before = events.iter().filter(|entry| entry.index < end);
    let scheduled: Vec<Seq> = before
        .clone()
        .filter_map(|entry| match &entry.event {
            Event::Command(Command::ScheduleCall { seq, .. }) => Some(*seq),
            _ => None,
        })
        .collect();
    before
        .filter_map(|entry| match &entry.event {
            Event::Completion(
                Completion::CallCompleted { seq, .. } | Completion::CallFailed { seq, .. },
            ) if scheduled.contains(seq) => Some(*seq),
            _ => None,
        })
        .collect()
}

/// The action of each call the history schedules.
fn actions_by_seq(events: &[HistoryEvent]) -> Vec<(Seq, String)> {
    events
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Command(Command::ScheduleCall { seq, action, .. }) => {
                Some((*seq, action.clone()))
            }
            _ => None,
        })
        .collect()
}
