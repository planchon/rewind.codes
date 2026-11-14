//! `rewind replay <run|journal.json> [--script fixed.ts [--at n]]`, in this process
//! (`archi/05-testing-and-observability.md`, "Replay, debugging and forks").
//!
//! The journal document is read from the file, or exported by the server
//! (`GET /v1/runs/{id}/journal`); either way the replay runs here, on a fresh V8 executor,
//! and calls no tool. Without `--script`, the run's own script must reproduce the journal
//! exactly: a divergence is non-determinism. With `--script`, the corrected script is followed
//! against the journal as a fork would follow it (`Engine::compatibility`): the report says
//! where it stops following the run, and which calls a fork would reuse, call again, or fail
//! with `outcome_unknown`.

use std::path::Path;
use std::time::Duration;

use rewind_api::runs::JournalExport;
use rewind_core::{
    Command, Completion, Event, HistoryEntry, JournalDocument, Lifecycle, NonDeterminism, RunState,
    Script, Seq, Timestamp,
};
use rewind_engine::adapters::{
    MemoryBlobs, MemoryJournal, MemorySecrets, MemoryWebhooks, SeededEntropy, VirtualClock,
};
use rewind_engine::mock::NoMocks;
use rewind_engine::ports::{
    Activation, CreateRun, JournalStore, NewRun, RunMetadata, RunRef, ScriptExecutor, TenantId,
    ToolDeclarations,
};
use rewind_engine::replay::{self, ReplayEnd, ReplayError, ReplayInput};
use rewind_engine::{Adapters, Compatibility, Engine, EngineConfig, ForkError, ForkPoint};
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
    pub(crate) entries: u64,
    /// The script reproduces the journal: its own script, or the corrected one up to the end
    /// of the run (or its cancellation or abort).
    pub(crate) matches: bool,
    /// What the replay found, in one sentence.
    pub(crate) verdict: String,
    /// With `--script`: what a fork with the corrected script would do.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) fork: Option<ForkReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ForkReport {
    /// The calls whose recorded outcome the fork reuses: they are not called again.
    pub(crate) reused: Vec<CallRef>,
    /// Calls the run left without an outcome, under `retry`: called again with the run's key.
    pub(crate) called_again: Vec<CallRef>,
    /// Calls the run left without an outcome, under `at_most_once`: they fail with
    /// `outcome_unknown`.
    pub(crate) outcome_unknown: Vec<CallRef>,
    /// The fork's status once it has followed the run.
    pub(crate) status: rewind_api::common::RunStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct CallRef {
    pub(crate) seq: u32,
    pub(crate) tool: String,
}

pub(super) async fn replay(
    server: &Server<'_>,
    args: ReplayArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let path = Path::new(&args.run);
    // A run id is a UUID: what looks like a path is a file, which must exist.
    let is_file = path.is_file()
        || args.run.contains(std::path::MAIN_SEPARATOR)
        || path
            .extension()
            .is_some_and(|extension| extension == "json");
    let journal = if is_file {
        let text = input::read_text(path)?;
        rewind_format::decode_journal(&text)
            .map_err(|error| CliError::usage(format!("{}: {error}", path.display())))?
    } else {
        let client = server.client().await?;
        let document = client
            .export_journal(&args.run, JournalExport::Journal)
            .await?;
        rewind_format::decode_journal(&document.to_string()).map_err(|error| {
            CliError::Api(rewind_api::Error::Decode(format!(
                "the exported journal of run {}: {error}",
                args.run
            )))
        })?
    };
    let report = match &args.script {
        None => replay_journal(&journal).await?,
        Some(path) => {
            let script = Script::new(input::read_text(path)?);
            compatibility(&journal, &script, args.at).await?
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
                    .map(|call| format!("#{} {}", call.seq, call.tool))
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
                    "in a fork, these fail with outcome_unknown (at_most_once): {}",
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

/// Replays a journal with its own script.
pub(crate) async fn replay_journal(journal: &JournalDocument) -> Result<ReplayReport, CliError> {
    let entries = journal.entries.len() as u64;
    let mut report = ReplayReport {
        run: journal.run.to_string(),
        entries,
        matches: false,
        verdict: String::new(),
        fork: None,
    };
    if let Err(error) = journal.check() {
        report.verdict = format!("the journal is inconsistent: {error}");
        return Ok(report);
    }
    let executor = executor()?;
    let compiled = executor.compile(&journal.script, &journal.toolset);
    let run = RunRef::new(TenantId::local(), journal.run);
    let input = ReplayInput {
        run: &run,
        compiled: compiled.as_ref(),
        toolset: &journal.toolset,
        history: &journal.entries,
    };
    match replay::replay(&executor, &MemoryBlobs::new(), input).await {
        Ok(rebuilt) => {
            report.matches = true;
            report.verdict = match rebuilt.end {
                ReplayEnd::EndOfHistory => format!(
                    "the script replays the {entries} entries of the journal of run {}",
                    journal.run
                ),
                ReplayEnd::Aborted(reason) => format!(
                    "the script replays the {entries} entries of the journal of run {}, up to its abort ({reason})",
                    journal.run
                ),
            };
        }
        Err(ReplayError::NonDeterminism(mismatch)) => {
            report.verdict = format!("the replay diverges from the journal: {mismatch}");
        }
        Err(error) => {
            report.verdict = format!("the replay fails: {error}");
        }
    }
    Ok(report)
}

type LocalEngine<'a> = Engine<
    MemoryJournal,
    MemoryBlobs,
    &'a V8Executor,
    NoMocks,
    VirtualClock,
    SeededEntropy,
    MemorySecrets,
    MemoryWebhooks,
>;

/// Follows a journal with a corrected script, as a fork would.
pub(crate) async fn compatibility(
    journal: &JournalDocument,
    script: &Script,
    at: Option<u64>,
) -> Result<ReplayReport, CliError> {
    let entries = journal.entries.len() as u64;
    let mut report = ReplayReport {
        run: journal.run.to_string(),
        entries,
        matches: false,
        verdict: String::new(),
        fork: None,
    };
    if let Err(error) = journal.check() {
        report.verdict = format!("the journal is inconsistent: {error}");
        return Ok(report);
    }
    let executor = executor()?;
    let now = journal
        .entries
        .last()
        .map_or(Timestamp(0), |entry| entry.recorded_at);
    let engine: LocalEngine<'_> = Engine::new(
        Adapters {
            journal: MemoryJournal::new(),
            blobs: MemoryBlobs::new(),
            executor: &executor,
            dispatcher: NoMocks,
            clock: VirtualClock::new(now),
            entropy: SeededEntropy::new(0),
            secrets: MemorySecrets::new(),
            webhooks: MemoryWebhooks::new(),
        },
        EngineConfig::default(),
    );
    let run = load(engine.journal(), journal, now).await?;
    let found: Compatibility = match engine.compatibility(&run, script, at).await {
        Ok(found) => found,
        Err(ForkError::Start(refused)) => {
            return Err(CliError::ScriptRefused {
                message: format!("the corrected script cannot start: {refused}"),
                diagnostics: Vec::new(),
            });
        }
        Err(error) => {
            return Err(CliError::internal(format!(
                "the journal cannot be followed: {error}"
            )));
        }
    };
    report.matches = found.is_compatible();
    report.verdict = match &found.point {
        ForkPoint::Diverged(mismatch) => {
            format!("the script diverges from the journal: {mismatch}")
        }
        ForkPoint::At(index) => format!(
            "the script follows the journal of run {} up to entry {index}, where a fork goes live",
            journal.run
        ),
        ForkPoint::Stopped => {
            "the script could not go on while it followed the journal: a fork is aborted".to_owned()
        }
        point @ (ForkPoint::EndOfParent
        | ForkPoint::ParentCancelled(_)
        | ForkPoint::ParentAborted(_)) => format!(
            "the script is compatible with the journal of run {} ({entries} entries): {point}",
            journal.run
        ),
    };
    let tools = tools_by_seq(&journal.entries);
    // What a fork reuses: the calls it copies with their outcomes while it follows the run,
    // and the calls it inherits still pending, whose recorded outcomes it delivers.
    let mut reused = completed_before(&journal.entries, live_from(&found.point, entries));
    reused.extend(found.reused.iter().copied());
    reused.sort();
    reused.dedup();
    let calls = |seqs: &[Seq]| {
        seqs.iter()
            .map(|seq| CallRef {
                seq: seq.0,
                tool: tools
                    .iter()
                    .find(|(called, _)| called == seq)
                    .map(|(_, tool)| tool.clone())
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

/// The entry where a fork stops following the run.
fn live_from(point: &ForkPoint, entries: u64) -> u64 {
    match point {
        ForkPoint::EndOfParent => entries,
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

/// The calls scheduled and completed before entry `end`.
fn completed_before(entries: &[HistoryEntry], end: u64) -> Vec<Seq> {
    let before = entries.iter().filter(|entry| entry.index < end);
    let scheduled: Vec<Seq> = before
        .clone()
        .filter_map(|entry| match &entry.event {
            Event::Command(Command::ScheduleToolCall { seq, .. }) => Some(*seq),
            _ => None,
        })
        .collect();
    before
        .filter_map(|entry| match &entry.event {
            Event::Completion(
                Completion::ToolCallCompleted { seq, .. } | Completion::ToolCallFailed { seq, .. },
            ) if scheduled.contains(seq) => Some(*seq),
            _ => None,
        })
        .collect()
}

/// The tool of each call the journal schedules.
fn tools_by_seq(entries: &[HistoryEntry]) -> Vec<(Seq, String)> {
    entries
        .iter()
        .filter_map(|entry| match &entry.event {
            Event::Command(Command::ScheduleToolCall { seq, tool, .. }) => {
                Some((*seq, tool.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Puts a journal document in a store of its own, as the run it records: its toolset, the run
/// created from its `run_started`, and its history committed at once.
async fn load(
    store: &MemoryJournal,
    journal: &JournalDocument,
    now: Timestamp,
) -> Result<RunRef, CliError> {
    let fail = |error: &dyn std::fmt::Display| {
        CliError::internal(format!(
            "cannot load the journal of run {}: {error}",
            journal.run
        ))
    };
    let Some(HistoryEntry {
        event: Event::Lifecycle(Lifecycle::RunStarted(started)),
        recorded_at,
        ..
    }) = journal.entries.first()
    else {
        return Err(fail(&"it does not start with run_started"));
    };
    let tenant = TenantId::local();
    let run = RunRef::new(tenant.clone(), journal.run);
    store
        .put_toolset(&tenant, &journal.toolset, &ToolDeclarations::default())
        .await
        .map_err(|error| fail(&error))?;
    let created = store
        .create_run(
            NewRun {
                run: run.clone(),
                // A local replay, in the embedded server's namespace.
                namespace: rewind_engine::ports::NamespaceName::new(
                    rewind_engine::ports::NamespaceName::LOCAL,
                )
                .map_err(|error| fail(&error))?,
                started: started.clone(),
                metadata: RunMetadata::default(),
                created_at: *recorded_at,
            },
            None,
        )
        .await
        .map_err(|error| fail(&error))?;
    if !matches!(created, CreateRun::Created(_)) {
        return Err(fail(&"the store refused the run"));
    }
    let lease = store
        .acquire_run(&run, "rewind-cli", now, Duration::from_secs(60))
        .await
        .map_err(|error| fail(&error))?
        .ok_or_else(|| fail(&"the run cannot be leased"))?;
    let consumed = store
        .take_inbox(&lease)
        .await
        .map_err(|error| fail(&error))?
        .into_iter()
        .map(|item| item.id)
        .collect();
    let status = RunState::from_history(&journal.entries)
        .map_err(|error| fail(&error))?
        .status();
    store
        .commit_activation(
            &lease,
            Activation::new(journal.entries.clone(), consumed, status),
            now,
        )
        .await
        .map_err(|error| fail(&error))?;
    if !status.is_terminal() {
        store.release(&lease).await.map_err(|error| fail(&error))?;
    }
    Ok(run)
}
