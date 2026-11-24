//! The engine's state as the HTTP API `/v1` shows it (`rewind-api`'s DTOs), built as `rewind-server`
//! builds it (`crates/rewind-server/src/views.rs`, `error.rs`, `http/action_sets.rs`,
//! `http/search.rs`), so that the agent tools render what they render from the server's answers:
//! a run from its head and its history, a run of a list from its summary.
//!
//! The evals cannot depend on `rewind-server`, a composition root: what follows repeats its
//! rules. Two differences: `deliveries` stays empty (the SDK's texts do not show it), and the call
//! whose `ActionError` failed the run is found by the message the runtime writes, `{action} failed:
//! {message}`.

use std::collections::BTreeMap;

use rewind_api::calls::{CallState, ParkedReason};
use rewind_api::checks::{CheckResult, Diagnostic, Location, Severity};
use rewind_api::common::{
    AbortReason, ActionFailure, Callsite, Labels, LogLevel, PAYLOAD_VALUE_LIMIT, Payload,
    RunStatus, timestamp,
};
use rewind_api::common::{ErrorInfo, FailedCall};
use rewind_api::runs::{
    ForkSource, LogLine, Quarantine, QuarantineCause, Run, RunSummary, RunWait, WaitKind, Waiting,
};
use rewind_core::{
    Annotation, Command, Completion, Event, HistoryEvent, Lifecycle, RunOutcome, RunState, Seq,
};
use rewind_engine::edge::EdgeError;
use rewind_engine::ports::link::Refusal;
use rewind_engine::ports::{
    CatalogStore, InvalidCode, NamespaceName, ParkKind, QuarantineKind, RunRef, StoredVersion,
    StoredWorkflow,
};

use crate::world::World;

/// The last log lines a run's state carries.
const LOG_LINES: usize = 20;

fn payload(payload: &rewind_core::Payload) -> Payload {
    match payload {
        rewind_core::Payload::Value(value) => Payload::of(value.clone(), PAYLOAD_VALUE_LIMIT),
        rewind_core::Payload::Digest(digest) => Payload::reference(digest.to_hex(), 0, ""),
    }
}

fn unavailable(message: String) -> EdgeError {
    EdgeError::Unavailable(message)
}

/// `GET /v1/runs/{id}`.
pub async fn run(world: &World, run: &RunRef) -> Result<Run, EdgeError> {
    let view = world
        .run_view(run)
        .await?
        .ok_or(Refusal::RunNotFound(run.id))?;
    let record = view.head.clone();
    // A run named by a handle, or by its id once it is erased.
    let named = |handle: Option<&rewind_core::RunHandle>, run: rewind_core::RunId| {
        handle.map_or_else(|| run.to_string(), ToString::to_string)
    };
    let workflow = record.metadata.workflow.as_ref().map(run_workflow);
    // Every payload loaded, as the server loads those it shows.
    let history = world.history(run).await?;
    let state = RunState::from_history(&history).map_err(|error| {
        unavailable(format!("the history of run {} is corrupt: {error}", run.id))
    })?;
    // The status its owner computed.
    let status: RunStatus = record.status.into();
    let started = history.first().and_then(|entry| match &entry.event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(started),
        _ => None,
    });
    let mut output = None;
    let mut error = None;
    let mut abort = None;
    match state.outcome() {
        Some(RunOutcome::Completed { output: value }) => output = Some(payload(value)),
        Some(RunOutcome::Failed { error: failure }) => {
            let source = world
                .catalog()
                .get_code(&run.tenant, record.code)
                .await
                .map_err(|error| unavailable(error.to_string()))?
                .map(|stored| stored.code.source().to_owned())
                .unwrap_or_default();
            error = Some(run_error(failure, &history, &source));
        }
        Some(RunOutcome::Aborted { reason }) => abort = Some(AbortReason::from(reason)),
        None => {}
    }
    let waiting_for = if status != RunStatus::Waiting || state.is_ended() || history.is_empty() {
        Vec::new()
    } else {
        waiting(world, run, &history, &state).await?
    };
    Ok(Run {
        id: run.id.to_string(),
        handle: record.handle().to_string(),
        namespace: namespace_of(run, &record.namespace)?,
        status,
        quarantine: record.quarantine.as_ref().map(|quarantine| Quarantine {
            cause: match quarantine.kind {
                QuarantineKind::NonDeterminism => QuarantineCause::NonDeterminism,
                QuarantineKind::Failures => QuarantineCause::RepeatedFailures,
            },
            reason: quarantine.reason.clone(),
        }),
        code_digest: record.code.to_hex(),
        action_set: record.action_set.to_hex(),
        epoch: record.epoch.0,
        mode: started.map_or(rewind_api::common::RunMode::Live, |started| {
            started.mode.into()
        }),
        labels: Labels::from(record.metadata.labels.clone()),
        secrets: record
            .metadata
            .secrets
            .iter()
            .map(|(server, secret)| (server.to_owned(), secret.to_string()))
            .collect(),
        trace_id: rewind_engine::telemetry::trace_of(run.id, &record.metadata)
            .0
            .to_string(),
        source: record.metadata.source.map(|parent| ForkSource {
            run: parent.run.to_string(),
            handle: named(view.source.as_ref(), parent.run),
            at: parent.at,
        }),
        workflow,
        started_by: record
            .metadata
            .started_by
            .map(|by| rewind_api::workflows::StartedBy {
                run: by.run.to_string(),
                handle: named(view.started_by.as_ref(), by.run),
                seq: by.seq.0,
                detached: by.detached,
            }),
        input: started.map(|started| payload(&started.input)),
        output,
        error,
        abort,
        waiting_for,
        logs: logs(&history),
        deliveries: Vec::new(),
        history_len: (history.len() as u64).max(record.history_len),
        created_at: timestamp(record.created_at),
        updated_at: timestamp(record.updated_at),
        ended_at: record.ended_at.map(timestamp),
        expires_at: timestamp(record.expires_at),
    })
}

/// The workflow a run is a run of, as the API shows it (`rewind-server`'s
/// `views::run_workflow`).
pub fn run_workflow(
    workflow: &rewind_engine::ports::RunWorkflow,
) -> rewind_api::workflows::RunWorkflow {
    rewind_api::workflows::RunWorkflow {
        id: workflow.id.to_string(),
        name: workflow.name.clone(),
        version: workflow.version,
        digest: workflow.digest.to_hex(),
    }
}

async fn waiting(
    world: &World,
    run: &RunRef,
    history: &[HistoryEvent],
    state: &RunState,
) -> Result<Vec<Waiting>, EdgeError> {
    let mut calls = BTreeMap::new();
    let mut timers = BTreeMap::new();
    for entry in history {
        match &entry.event {
            Event::Command(Command::ScheduleCall {
                seq,
                args,
                callsite,
                ..
            }) => {
                calls.insert(*seq, (args, callsite.map(Callsite::from)));
            }
            Event::Command(Command::StartTimer { seq, callsite, .. }) => {
                timers.insert(*seq, callsite.map(Callsite::from));
            }
            _ => {}
        }
    }
    let tasks: BTreeMap<Seq, _> = world
        .call_tasks(run)
        .await?
        .into_iter()
        .map(|task| (task.task.seq, task))
        .collect();
    let mut waiting = Vec::new();
    for (seq, action) in state.pending_calls() {
        let (args, callsite) = calls.get(seq).map_or((None, None), |(args, callsite)| {
            (Some(payload(args)), *callsite)
        });
        let task = tasks.get(seq);
        let (task_state, parked) = match task.map(|task| &task.state) {
            None => (CallState::Done, None),
            Some(rewind_engine::ports::TaskState::Approval) => (CallState::Approval, None),
            Some(rewind_engine::ports::TaskState::Ready { .. }) => (CallState::Ready, None),
            Some(rewind_engine::ports::TaskState::Claimed { .. }) => (CallState::Claimed, None),
            Some(rewind_engine::ports::TaskState::Deferred { .. }) => (CallState::Deferred, None),
            Some(rewind_engine::ports::TaskState::Parked(reason)) => (
                CallState::Parked,
                Some(match reason.kind {
                    ParkKind::SchemaChanged => ParkedReason::SchemaChanged,
                    ParkKind::CredentialsRefused => ParkedReason::CredentialsRefused,
                    ParkKind::SecretMissing => ParkedReason::SecretMissing,
                }),
            ),
        };
        waiting.push(Waiting::Call {
            seq: seq.0,
            action: action.clone(),
            args: args.unwrap_or_else(|| Payload::reference("", 0, "")),
            call_id: format!("{}/{}", run.id, seq.0),
            state: task_state,
            parked,
            delivery: task.map_or(1, |task| task.delivery.max(1)),
            callsite,
        });
    }
    let timeouts: Vec<Seq> = state
        .awaited_signals()
        .iter()
        .filter_map(|wait| wait.timeout)
        .collect();
    for (seq, fire_at) in state.pending_timers() {
        if timeouts.contains(seq) {
            continue;
        }
        waiting.push(Waiting::Timer {
            seq: seq.0,
            fire_at: timestamp(*fire_at),
            callsite: timers.get(seq).copied().flatten(),
        });
    }
    for wait in state.awaited_signals() {
        let callsite = history
            .iter()
            .find(|entry| entry.index == wait.since)
            .and_then(|entry| match &entry.event {
                Event::Annotation(Annotation::SignalAwaited { callsite, .. }) => *callsite,
                _ => None,
            })
            .map(Callsite::from);
        waiting.push(Waiting::Signal {
            name: wait.name.clone(),
            timeout_at: wait
                .timeout
                .and_then(|timer| state.pending_timers().get(&timer))
                .map(|at| timestamp(*at)),
            callsite,
        });
    }
    Ok(waiting)
}

/// The last log lines of the code.
fn logs(history: &[HistoryEvent]) -> Vec<LogLine> {
    let mut lines: Vec<LogLine> = history
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
    let excess = lines.len().saturating_sub(LOG_LINES);
    lines.drain(..excess);
    lines
}

/// A run in `GET /v1/runs`, from its summary (`rewind-server`'s `http/search.rs`, `summary`).
pub fn summary(run: &rewind_engine::ports::RunSummary) -> Result<RunSummary, EdgeError> {
    let ending = run.ending.as_ref();
    Ok(RunSummary {
        id: run.run.id.to_string(),
        handle: run.handle().to_string(),
        namespace: namespace_of(&run.run, &run.namespace)?,
        status: run.status.into(),
        quarantined: run.quarantine.is_some(),
        code_digest: run.code.to_hex(),
        action_set: run.action_set.to_hex(),
        labels: Labels::from(run.labels.clone()),
        started_at: timestamp(run.started_at.unwrap_or(run.last_activity_at)),
        ended_at: run.ended_at.map(timestamp),
        last_activity_at: timestamp(run.last_activity_at),
        calls: run.calls,
        errors: run.errors,
        history_len: run.history_len,
        error: ending.map(|ending| ending.error.clone()),
        error_message: ending.map(|ending| ending.message.clone()),
        error_line: ending.and_then(|ending| ending.line),
        fingerprint: ending.map(|ending| ending.fingerprint.clone()),
        waiting_for: run
            .waits
            .iter()
            .filter(|_| run.status == rewind_core::RunStatus::Waiting)
            .filter_map(|wait| Some((wait, wait.waiting_for()?)))
            .map(|(wait, kind)| RunWait {
                kind: wait_kind(kind),
                seq: wait.seq.map(|seq| seq.0),
                name: wait.name.clone(),
                since: timestamp(wait.since),
                until: wait.until.map(timestamp),
                callsite: wait.callsite.map(Callsite::from),
            })
            .collect(),
        wakes_at: run.wakes_at.map(timestamp),
        source: run.source.map(|parent| parent.to_string()),
        workflow: run.workflow.as_ref().map(run_workflow),
        started_by: run.started_by.map(|run| run.to_string()),
    })
}

/// What a wait is, as the API names it.
fn wait_kind(waiting: rewind_engine::ports::WaitingFor) -> WaitKind {
    use rewind_engine::ports::WaitingFor;
    match waiting {
        WaitingFor::Signal => WaitKind::Signal,
        WaitingFor::Timer => WaitKind::Timer,
        WaitingFor::Harness => WaitKind::Harness,
        WaitingFor::Approval => WaitKind::Approval,
        WaitingFor::Parked => WaitKind::Parked,
        WaitingFor::Deferred => WaitKind::Deferred,
        WaitingFor::Quarantined => WaitKind::Quarantined,
    }
}

/// How the code failed, with where: the first position in `workflow.ts` of its stack, else of
/// its message, and the call whose `ActionError` it did not catch.
fn run_error(error: &rewind_core::ErrorInfo, history: &[HistoryEvent], source: &str) -> ErrorInfo {
    let location = error
        .stack
        .as_deref()
        .and_then(code_location)
        .or_else(|| code_location(&error.message));
    ErrorInfo {
        name: error.name.clone(),
        message: error.message.clone(),
        stack: error.stack.clone(),
        frame: location.and_then(|(line, column)| code_frame(source, line, column)),
        location: location.map(|(line, column)| Callsite { line, column }),
        failed_call: error
            .failed_call(history)
            .and_then(|seq| failed_call(history, seq)),
    }
}

/// The call whose `ActionError` the code did not catch, as `ErrorInfo::failed_call` finds it.
fn failed_call(history: &[HistoryEvent], seq: Seq) -> Option<FailedCall> {
    let failure = history.iter().find_map(|entry| match &entry.event {
        Event::Completion(Completion::CallFailed { seq: failed, error }) if *failed == seq => {
            Some(error)
        }
        _ => None,
    })?;
    history.iter().find_map(|entry| match &entry.event {
        Event::Command(Command::ScheduleCall {
            seq: scheduled,
            action,
            args,
            callsite,
        }) if *scheduled == seq => Some(FailedCall {
            seq: seq.0,
            action: action.clone(),
            args: payload(args),
            error: ActionFailure::from(failure),
            callsite: callsite.map(Callsite::from),
        }),
        _ => None,
    })
}

/// The first `workflow.ts:<line>:<column>` of a message or a stack.
pub fn code_location(text: &str) -> Option<(u32, u32)> {
    let mut rest = text;
    while let Some(at) = rest.find("workflow.ts:") {
        rest = &rest[at + "workflow.ts:".len()..];
        let mut parts = rest.splitn(3, ':');
        let line = parts.next().and_then(|line| line.parse().ok());
        let column = parts.next().and_then(|column| {
            let digits: String = column.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().ok()
        });
        if let (Some(line), Some(column)) = (line, column) {
            return Some((line, column));
        }
    }
    None
}

/// A few lines of the source around a position, the column marked.
pub fn code_frame(source: &str, line: u32, column: u32) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let index = usize::try_from(line).ok()?.checked_sub(1)?;
    let target = lines.get(index)?;
    let width = (index + 2).to_string().len();
    let mut frame = String::new();
    if index > 0 && !lines[index - 1].trim().is_empty() {
        frame.push_str(&format!("{:>width$} | {}\n", index, lines[index - 1]));
    }
    frame.push_str(&format!("{:>width$} | {target}\n", index + 1));
    let caret = usize::try_from(column.max(1) - 1).unwrap_or(0);
    frame.push_str(&format!("{:>width$} | {}^", "", " ".repeat(caret)));
    if let Some(next) = lines.get(index + 1).filter(|next| !next.trim().is_empty()) {
        frame.push_str(&format!("\n{:>width$} | {next}", index + 2));
    }
    Some(frame)
}

/// A diagnostic of the checker, as the API writes it.
pub fn diagnostic(diagnostic: &rewind_engine::ports::Diagnostic) -> Diagnostic {
    Diagnostic {
        severity: match diagnostic.severity {
            rewind_engine::ports::Severity::Error => Severity::Error,
            rewind_engine::ports::Severity::Warning => Severity::Warning,
        },
        code: match &diagnostic.source {
            rewind_engine::ports::DiagnosticSource::TypeScript { code } => format!("TS{code}"),
            rewind_engine::ports::DiagnosticSource::Rule(rule) => format!("rewind/{rule}"),
        },
        message: diagnostic.message.clone(),
        location: (diagnostic.line > 0).then_some(Location {
            line: diagnostic.line,
            column: diagnostic.column,
            end_line: None,
            end_column: None,
        }),
        frame: (!diagnostic.frame.is_empty()).then(|| diagnostic.frame.clone()),
        hint: diagnostic.hint.clone(),
    }
}

/// The checks at start refused the code: one diagnostic, located where the message says.
pub fn invalid_code_diagnostic(invalid: &InvalidCode) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        code: "rewind/invalid-code".into(),
        message: invalid.message.clone(),
        location: code_location(&invalid.message).map(|(line, column)| Location {
            line,
            column,
            end_line: None,
            end_column: None,
        }),
        frame: None,
        hint: None,
    }
}

/// `POST /v1/check`, from the checker's diagnostics.
pub fn check_result(
    diagnostics: &[rewind_engine::ports::Diagnostic],
    workflow: Option<rewind_api::workflows::RunWorkflow>,
) -> CheckResult {
    let diagnostics: Vec<Diagnostic> = diagnostics.iter().map(diagnostic).collect();
    CheckResult {
        ok: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error),
        diagnostics,
        workflow,
    }
}

/// The run's namespace as the API names it: the engine and the API check the same rule.
fn namespace_of(
    run: &RunRef,
    namespace: &NamespaceName,
) -> Result<rewind_api::namespaces::NamespaceName, EdgeError> {
    rewind_api::namespaces::NamespaceName::new(namespace.as_str())
        .map_err(|_| unavailable(format!("run {} has an invalid namespace", run.id)))
}

/// A workflow, as `GET /v1/workflows/{id}` shows it (`rewind-server`'s `http/workflows.rs`,
/// `view`): `shown`, the version it shows with its code, its versions the agent sees and its
/// suite.
pub async fn workflow(
    world: &World,
    workflow: &StoredWorkflow,
    shown: &StoredVersion,
) -> Result<rewind_api::workflows::Workflow, EdgeError> {
    use rewind_api::workflows::{CodeComment, SuiteEntry, Workflow, WorkflowSource};

    let tenant = rewind_engine::ports::TenantId::local();
    let catalog = world.catalog();
    let failed = |error: rewind_engine::ports::CatalogError| unavailable(error.to_string());
    let code = catalog
        .get_code(&tenant, shown.document.code())
        .await
        .map_err(failed)?
        .map(|stored| stored.code.source().to_owned())
        .unwrap_or_default();
    let comment = rewind_core::code_doc(&code).map(|doc| CodeComment {
        title: doc.title,
        description: doc.description,
    });
    let versions = catalog
        .versions(&tenant, &workflow.id, &world.labels)
        .await
        .map_err(failed)?
        .iter()
        .map(|version| version_view(workflow, version, None))
        .collect();
    let suite = catalog
        .suite(&tenant, &workflow.id)
        .await
        .map_err(failed)?
        .into_iter()
        .map(|member| SuiteEntry {
            name: member.name,
            digest: member.digest.to_hex(),
            added_at: timestamp(member.added_at),
            retired_at: member.retired_at.map(timestamp),
            reason: member.reason,
        })
        .collect();
    Ok(Workflow {
        id: workflow.id.to_string(),
        short_id: workflow.id.short(),
        name: workflow.name.clone(),
        description: workflow.description.clone(),
        namespace: workflow.scope.namespace().and_then(|namespace| {
            rewind_api::namespaces::NamespaceName::new(namespace.as_str()).ok()
        }),
        source: workflow.source.as_ref().map(|source| WorkflowSource {
            workflow: source.workflow.to_string(),
            version: source.version,
        }),
        labels: Labels::from(workflow.labels.clone()),
        created_at: timestamp(workflow.created_at),
        promoted_at: workflow.promoted_at.map(timestamp),
        version: version_view(workflow, shown, comment),
        code,
        versions,
        suite,
    })
}

/// A version as the API shows it, with the suite it passed.
fn version_view(
    workflow: &StoredWorkflow,
    stored: &StoredVersion,
    comment: Option<rewind_api::workflows::CodeComment>,
) -> rewind_api::workflows::WorkflowVersion {
    use rewind_api::workflows::{ReplacedTest, RetiredTest, SuiteTest, WorkflowVersion};

    let document = &stored.document;
    let schemas = document.interface();
    let declarations = match (&workflow.name, schemas) {
        (Some(name), Some(schemas)) => {
            let pinned = rewind_core::PinnedWorkflow {
                name: name.clone(),
                id: workflow.id.clone(),
                version: stored.version,
                digest: stored.digest,
                description: workflow.description.clone(),
                schemas: schemas.clone(),
            };
            let action = format!("{}.{name}", rewind_core::WORKFLOWS_GROUP);
            rewind_core::ActionSet::empty()
                .with_workflows([pinned])
                .ok()
                .map(|action_set| rewind_codegen::generate(&action_set, &BTreeMap::new()))
                .and_then(|generated| {
                    generated
                        .actions
                        .iter()
                        .find(|declaration| declaration.name == action)
                        .map(|declaration| rewind_api::action_sets::Declarations {
                            dts: declaration.typescript.clone(),
                            prompt: declaration.prompt.clone(),
                        })
                })
        }
        _ => None,
    };
    let suite = document.suite();
    WorkflowVersion {
        version: stored.version,
        digest: stored.digest.to_hex(),
        code_digest: document.code().to_hex(),
        comment,
        action_set: document.action_set().to_hex(),
        input: schemas.map(|schemas| schemas.input.clone()),
        output: schemas.map(|schemas| schemas.output.clone()),
        signals: schemas.map(|schemas| schemas.signals.clone()),
        labels: Labels::from(stored.labels.clone()),
        secrets: document
            .secrets()
            .iter()
            .map(|(server, secret)| (server.to_owned(), secret.to_string()))
            .collect(),
        created_at: timestamp(stored.created_at),
        declarations,
        base: suite.base.map(|base| base.to_hex()),
        tests: suite
            .tests
            .iter()
            .map(|test| SuiteTest {
                name: test.name.clone(),
                digest: test.test.to_hex(),
            })
            .collect(),
        retired: suite
            .retired
            .iter()
            .map(|retired| RetiredTest {
                name: retired.name.clone(),
                digest: retired.test.to_hex(),
                reason: retired.reason.clone(),
            })
            .collect(),
        replaced: suite
            .replaced
            .iter()
            .map(|replaced| ReplacedTest {
                name: replaced.name.clone(),
                old: replaced.old.to_hex(),
                new: replaced.new.to_hex(),
                reason: replaced.reason.clone(),
            })
            .collect(),
    }
}

/// The payload an event holds, if any (`rewind-server`'s `views::payload_of`).
pub fn payload_of(event: &Event) -> Option<&rewind_core::Payload> {
    match event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(&started.input),
        Event::Command(Command::ScheduleCall { args, .. }) => Some(args),
        Event::Command(Command::CompleteRun { output }) => Some(output),
        Event::Completion(Completion::CallCompleted { result, .. }) => Some(result),
        Event::Completion(Completion::SignalReceived { payload, .. }) => Some(payload),
        _ => None,
    }
}

/// The servers of an action set, with their actions counted (`rewind-server`'s
/// `action_sets::servers`).
fn servers(action_set: &rewind_core::ActionSet) -> Vec<rewind_api::action_sets::ServerSummary> {
    use rewind_core::{ActionRoute, McpTransport};
    action_set
        .servers()
        .iter()
        .map(|server| rewind_api::action_sets::ServerSummary {
            name: server.name.clone(),
            transport: match server.transport {
                McpTransport::Http { .. } => rewind_api::action_sets::McpTransport::Http,
                McpTransport::Stdio { .. } => rewind_api::action_sets::McpTransport::Stdio,
            },
            auth: rewind_api::action_sets::Auth::of_core(&server.auth),
            actions: u32::try_from(
                action_set
                    .actions()
                    .iter()
                    .filter(|action| matches!(&action.route, ActionRoute::Mcp { server: of } if *of == server.name))
                    .count(),
            )
            .unwrap_or(u32::MAX),
        })
        .collect()
}

/// A stored action set, as `POST /v1/action-sets` shows it (`rewind-server`'s `action_sets::view`).
pub fn action_set(
    stored: &rewind_engine::ports::StoredActionSet,
) -> rewind_api::action_sets::ActionSet {
    use rewind_api::action_sets::{Action, ActionPolicy, Route};
    use rewind_core::ActionRoute;
    let action_set = &stored.action_set;
    rewind_api::action_sets::ActionSet {
        id: action_set.digest().to_hex(),
        servers: servers(action_set),
        // The evals' world has MCP servers only.
        apis: Vec::new(),
        actions: action_set
            .actions()
            .iter()
            .map(|action| Action {
                name: action.name.clone(),
                route: match action.route {
                    ActionRoute::Mcp { .. } => Route::Mcp,
                    ActionRoute::Api { .. } => Route::Api,
                    ActionRoute::Harness { .. } => Route::Harness,
                    ActionRoute::Builtin(_) => Route::Builtin,
                },
                server: match &action.route {
                    ActionRoute::Mcp { server } => Some(server.clone()),
                    _ => None,
                },
                api: match &action.route {
                    ActionRoute::Api { api } => Some(api.clone()),
                    _ => None,
                },
                description: action.description.clone().unwrap_or_default(),
                input_schema: action.input_schema.clone(),
                output_schema: action.output_schema.clone(),
                policy: ActionPolicy {
                    idempotent: Some(action.policy.idempotent),
                    approval: Some(action.policy.approval),
                    timeout: action.policy.execution_timeout.map(Into::into),
                    reconnect_timeout: Some(action.policy.reconnect_timeout.into()),
                    concurrency: Some(action.policy.concurrency),
                },
            })
            .collect(),
        workflows: action_set
            .workflows()
            .iter()
            .map(|workflow| rewind_api::workflows::PinnedWorkflow {
                name: workflow.name.clone(),
                id: workflow.id.to_string(),
                version: workflow.version,
                digest: workflow.digest.to_hex(),
                description: workflow.description.clone(),
            })
            .collect(),
        declarations: rewind_api::action_sets::Declarations {
            dts: stored.declarations.typescript.clone(),
            prompt: stored.declarations.prompt.clone(),
        },
        skipped: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_are_read_from_messages_and_stacks() {
        assert_eq!(
            code_location("RangeError: invalid amount -5\n    at main (file:///workflow.ts:5:31)"),
            Some((5, 31))
        );
        assert_eq!(
            code_location("cannot import \"node:fs\" (workflow.ts:1:16): workflows can only…"),
            Some((1, 16))
        );
        assert_eq!(code_location("ActionError: crm.get_order failed"), None);
    }

    #[test]
    fn frames_mark_the_column_between_their_neighbors() {
        let source = "import { crm } from \"rewind:actions\";\n\nexport default async function main() {\n  const order = await crm.get_order({ id: \"O-504\" });\n  return order;\n}\n";
        assert_eq!(
            code_frame(source, 4, 23).unwrap(),
            "3 | export default async function main() {\n4 |   const order = await crm.get_order({ id: \"O-504\" });\n  |                       ^\n5 |   return order;"
        );
        assert_eq!(code_frame(source, 40, 1), None);
    }
}
