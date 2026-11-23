//! The engine's state as the HTTP API `/v1` shows it (`rewind-api`'s DTOs), built as `rewind-server`
//! builds it (`crates/rewind-server/src/views.rs`, `error.rs`, `http/toolsets.rs`), so that the
//! agent tools render what they render from the server's answers.
//!
//! The evals cannot depend on `rewind-server`, a composition root: what follows repeats its
//! rules. Two differences: `attempts` stays empty (the server reads it from the read model,
//! which the world does not project, and the SDK's texts do not show it), and the call whose
//! `ToolError` failed the run is found by the message the runtime writes, `{tool} failed:
//! {message}`.

use std::collections::BTreeMap;

use rewind_api::common::{
    AbortReason, Callsite, Labels, LogLevel, PAYLOAD_VALUE_LIMIT, Payload, RunStatus, ToolFailure,
    timestamp,
};
use rewind_api::runs::{
    FailedCall, ForkParent, LogLine, Quarantine, QuarantineCause, Run, RunError, RunSummary,
    RunWait, WaitKind, Waiting,
};
use rewind_api::scripts::{CheckResult, Diagnostic, Location, Severity, TitleSource};
use rewind_api::tasks::{ParkedReason, TaskState};
use rewind_core::{
    Annotation, Command, Completion, Event, HistoryEntry, Lifecycle, RunOutcome, RunState, Seq,
};
use rewind_engine::EngineError;
use rewind_engine::ports::{
    InvalidScript, JournalStore, ParkKind, QuarantineKind, RunRef, StoredWorkflow,
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

/// `GET /v1/runs/{id}`.
pub async fn run(world: &World, run: &RunRef) -> Result<Run, EngineError> {
    let engine = &world.engine;
    let record = engine
        .get_run(run)
        .await?
        .ok_or(EngineError::RunNotFound(run.id))?;
    // Every payload loaded, as the server loads those it shows.
    let history = engine.export_history(run).await?;
    let state = RunState::from_history(&history).map_err(|error| EngineError::CorruptHistory {
        run_id: run.id,
        error,
    })?;
    let status = if history.is_empty() {
        RunStatus::Pending
    } else {
        state.status().into()
    };
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
            let source = engine
                .journal()
                .get_script(&run.tenant, record.script)
                .await?
                .map(|stored| stored.script.source().to_owned())
                .unwrap_or_default();
            error = Some(run_error(failure, &history, &source));
        }
        Some(RunOutcome::Aborted { reason }) => abort = Some(AbortReason::from(reason)),
        None => {}
    }
    let waiting_for = if state.is_terminal() || history.is_empty() {
        Vec::new()
    } else {
        waiting(world, run, &history, &state).await?
    };
    let (trace, _) = rewind_engine::telemetry::trace_of(&record);
    let script_title = script_title(world, &run.tenant, record.script)
        .await?
        .map(|(title, _, _)| title);
    let parent = match record.metadata.parent {
        Some(parent) => Some(ForkParent {
            run: parent.run.to_string(),
            handle: handle_of(world, &run.tenant, parent.run).await?,
            at: parent.at,
        }),
        None => None,
    };
    let started_by = match record.metadata.started_by {
        Some(by) => Some(rewind_api::workflows::StartedBy {
            run: by.run.to_string(),
            handle: handle_of(world, &run.tenant, by.run).await?,
            seq: by.seq.0,
            detached: by.detached,
        }),
        None => None,
    };
    let workflow = match &record.metadata.workflow {
        Some(workflow) => Some(rewind_api::workflows::RunWorkflow {
            name: workflow.name.clone(),
            version: workflow.version.to_hex(),
            number: engine
                .journal()
                .get_workflow(&run.tenant, workflow.version)
                .await?
                .map_or(0, |stored| stored.number),
        }),
        None => None,
    };
    Ok(Run {
        id: run.id.to_string(),
        handle: record.handle().to_string(),
        namespace: namespace_of(&record)?,
        status,
        quarantine: record.quarantine.as_ref().map(|quarantine| Quarantine {
            cause: match quarantine.kind {
                QuarantineKind::NonDeterminism => QuarantineCause::NonDeterminism,
                QuarantineKind::Failures => QuarantineCause::RepeatedFailures,
            },
            reason: quarantine.reason.clone(),
        }),
        script: record.script.to_hex(),
        script_title,
        toolset: record.toolset.to_hex(),
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
        trace_id: trace.to_string(),
        parent,
        workflow,
        started_by,
        input: started.map(|started| payload(&started.input)),
        output,
        error,
        abort,
        waiting_for,
        logs: logs(&history),
        attempts: Vec::new(),
        history_len: (history.len() as u64).max(record.history_len),
        created_at: timestamp(record.created_at),
        updated_at: timestamp(record.updated_at),
        ended_at: record.ended_at.map(timestamp),
        expires_at: timestamp(record.expires_at),
    })
}

/// The handle of another run of the tenant, which a run names; its id once it is erased.
async fn handle_of(
    world: &World,
    tenant: &rewind_engine::ports::TenantId,
    run: rewind_core::RunId,
) -> Result<String, EngineError> {
    Ok(world
        .engine
        .get_run(&RunRef::new(tenant.clone(), run))
        .await?
        .map_or_else(|| run.to_string(), |record| record.handle().to_string()))
}

/// A script's title, its description and where they come from, as the server shows them: the
/// title a person gave the script, else the comment at the top of its source.
pub async fn script_title(
    world: &World,
    tenant: &rewind_engine::ports::TenantId,
    script: rewind_core::Digest,
) -> Result<Option<(String, Option<String>, TitleSource)>, EngineError> {
    let journal = world.engine.journal();
    if let Some((_, given)) = journal
        .script_titles(tenant, &[script])
        .await?
        .into_iter()
        .next()
    {
        return Ok(Some((given.title, given.description, TitleSource::Person)));
    }
    Ok(journal
        .get_script(tenant, script)
        .await?
        .and_then(|stored| rewind_core::script_doc(stored.script.source()))
        .map(|doc| (doc.title, doc.description, TitleSource::Comment)))
}

async fn waiting(
    world: &World,
    run: &RunRef,
    history: &[HistoryEntry],
    state: &RunState,
) -> Result<Vec<Waiting>, EngineError> {
    let mut calls = BTreeMap::new();
    let mut timers = BTreeMap::new();
    for entry in history {
        match &entry.event {
            Event::Command(Command::ScheduleToolCall {
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
        .engine
        .tool_tasks(run)
        .await?
        .into_iter()
        .map(|task| (task.task.seq, task))
        .collect();
    let mut waiting = Vec::new();
    for (seq, tool) in state.pending_tool_calls() {
        let (args, callsite) = calls.get(seq).map_or((None, None), |(args, callsite)| {
            (Some(payload(args)), *callsite)
        });
        let task = tasks.get(seq);
        let (task_state, parked) = match task.map(|task| &task.state) {
            None => (TaskState::Done, None),
            Some(rewind_engine::ports::TaskState::Approval) => (TaskState::Approval, None),
            Some(rewind_engine::ports::TaskState::Ready { .. }) => (TaskState::Ready, None),
            Some(rewind_engine::ports::TaskState::Claimed { .. }) => (TaskState::Claimed, None),
            Some(rewind_engine::ports::TaskState::Deferred { .. }) => (TaskState::Deferred, None),
            Some(rewind_engine::ports::TaskState::Parked(reason)) => (
                TaskState::Parked,
                Some(match reason.kind {
                    ParkKind::SchemaChanged => ParkedReason::SchemaChanged,
                    ParkKind::CredentialsRefused => ParkedReason::CredentialsRefused,
                    ParkKind::SecretMissing => ParkedReason::SecretMissing,
                }),
            ),
        };
        waiting.push(Waiting::ToolCall {
            seq: seq.0,
            tool: tool.clone(),
            args: args.unwrap_or_else(|| Payload::reference("", 0, "")),
            task_id: format!("{}/{}", run.id, seq.0),
            state: task_state,
            parked,
            attempt: task.map_or(1, |task| task.attempt.max(1)),
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

/// The last log lines of the script.
fn logs(history: &[HistoryEntry]) -> Vec<LogLine> {
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
    let ending = state
        .as_ref()
        .and_then(RunState::outcome)
        .and_then(rewind_engine::failures::ending);
    let toolset = world
        .engine
        .journal()
        .get_toolset(&run.tenant, record.toolset)
        .await?
        .map(|stored| stored.toolset);
    let snapshot = rewind_engine::projector::run_snapshot(&history, toolset.as_ref());
    let waiting_for: Vec<RunWait> = snapshot
        .waits
        .iter()
        .map(|wait| RunWait {
            kind: wait_kind(wait.waiting_for()),
            seq: wait.seq.map(|seq| seq.0),
            name: wait.name.clone(),
            since: timestamp(wait.since),
            until: wait.until.map(timestamp),
            callsite: wait.callsite.map(Into::into),
        })
        .collect();
    let wakes_at = snapshot.waits.iter().filter_map(|wait| wait.until).min();
    let headline = world
        .engine
        .journal()
        .get_script(&run.tenant, record.script)
        .await?
        .map(|stored| rewind_api::scripts::headline(stored.script.source()))
        .unwrap_or_default();
    let script_title = script_title(world, &run.tenant, record.script)
        .await?
        .map(|(title, _, _)| title);
    Ok(RunSummary {
        id: run.id.to_string(),
        handle: record.handle().to_string(),
        namespace: namespace_of(&record)?,
        status: record.status.into(),
        quarantined: record.quarantine.is_some(),
        script: record.script.to_hex(),
        script_title,
        headline,
        toolset: record.toolset.to_hex(),
        labels: Labels::from(record.metadata.labels.clone()),
        started_at: timestamp(record.created_at),
        ended_at: record.ended_at.map(timestamp),
        last_activity_at: timestamp(record.updated_at),
        tool_calls,
        errors,
        history_len: record.history_len,
        error: ending.as_ref().map(|ending| ending.error.clone()),
        error_message: ending.as_ref().map(|ending| ending.message.clone()),
        error_line: ending.as_ref().and_then(|ending| ending.line),
        fingerprint: ending.map(|ending| ending.fingerprint),
        waiting_for,
        wakes_at: wakes_at.map(timestamp),
        parent: record.metadata.parent.map(|parent| parent.run.to_string()),
        workflow: record
            .metadata
            .workflow
            .as_ref()
            .map(|workflow| workflow.name.clone()),
        started_by: record.metadata.started_by.map(|by| by.run.to_string()),
    })
}

/// What a wait is, as the API names it.
fn wait_kind(waiting: rewind_engine::ports::WaitingFor) -> WaitKind {
    use rewind_engine::ports::WaitingFor;
    match waiting {
        WaitingFor::Signal => WaitKind::Signal,
        WaitingFor::Timer => WaitKind::Timer,
        WaitingFor::Call => WaitKind::Call,
        WaitingFor::Host => WaitKind::Host,
        WaitingFor::Approval => WaitKind::Approval,
        WaitingFor::Parked => WaitKind::Parked,
        WaitingFor::Deferred => WaitKind::Deferred,
    }
}

/// How the script failed, with where: the first position in `script.ts` of its stack, else of
/// its message, and the call whose `ToolError` it did not catch.
fn run_error(error: &rewind_core::ScriptError, history: &[HistoryEntry], source: &str) -> RunError {
    let location = error
        .stack
        .as_deref()
        .and_then(script_location)
        .or_else(|| script_location(&error.message));
    RunError {
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

/// The call whose `ToolError` the script did not catch, as `ScriptError::failed_call` finds it.
fn failed_call(history: &[HistoryEntry], seq: Seq) -> Option<FailedCall> {
    let failure = history.iter().find_map(|entry| match &entry.event {
        Event::Completion(Completion::ToolCallFailed { seq: failed, error }) if *failed == seq => {
            Some(error)
        }
        _ => None,
    })?;
    history.iter().find_map(|entry| match &entry.event {
        Event::Command(Command::ScheduleToolCall {
            seq: scheduled,
            tool,
            args,
            callsite,
        }) if *scheduled == seq => Some(FailedCall {
            seq: seq.0,
            tool: tool.clone(),
            args: payload(args),
            error: ToolFailure::from(failure),
            callsite: callsite.map(Callsite::from),
        }),
        _ => None,
    })
}

/// The first `script.ts:<line>:<column>` of a message or a stack.
pub fn script_location(text: &str) -> Option<(u32, u32)> {
    let mut rest = text;
    while let Some(at) = rest.find("script.ts:") {
        rest = &rest[at + "script.ts:".len()..];
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

/// The checks at start refused the script: one diagnostic, located where the message says.
pub fn invalid_script_diagnostic(invalid: &InvalidScript) -> Diagnostic {
    Diagnostic {
        severity: Severity::Error,
        code: "rewind/invalid-script".into(),
        message: invalid.message.clone(),
        location: script_location(&invalid.message).map(|(line, column)| Location {
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
pub fn check_result(diagnostics: &[rewind_engine::ports::Diagnostic]) -> CheckResult {
    let diagnostics: Vec<Diagnostic> = diagnostics.iter().map(diagnostic).collect();
    CheckResult {
        ok: !diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error),
        diagnostics,
    }
}

/// The run's namespace as the API names it: the engine and the API check the same rule.
fn namespace_of(
    record: &rewind_engine::ports::RunRecord,
) -> Result<rewind_api::namespaces::NamespaceName, EngineError> {
    rewind_api::namespaces::NamespaceName::new(record.namespace.as_str()).map_err(|_| {
        EngineError::Store(rewind_engine::ports::StoreError::Backend(format!(
            "run {} has an invalid namespace",
            record.run.id
        )))
    })
}

/// A workflow version, as `GET /v1/workflows/{name}` shows it (`rewind-server`'s
/// `http/workflows.rs`, `view`): its declaration generated as a toolset that pins it alone
/// generates it.
pub fn workflow(stored: &StoredWorkflow) -> rewind_api::workflows::Workflow {
    use rewind_api::workflows::{RetiredTest, SuiteTest, UpdatedTest};

    let workflow = &stored.workflow;
    let declarations = rewind_core::Toolset::empty()
        .with_workflows([workflow.pinned()])
        .map(|toolset| rewind_codegen::generate(&toolset, &BTreeMap::new()))
        .ok();
    let name = format!("{}.{}", rewind_core::WORKFLOWS_NAMESPACE, workflow.name());
    let declaration = declarations
        .as_ref()
        .and_then(|generated| generated.tools.iter().find(|tool| tool.name == name));
    rewind_api::workflows::Workflow {
        name: workflow.name().to_owned(),
        version: workflow.version().to_hex(),
        number: stored.number,
        description: workflow.description().map(str::to_owned),
        toolset: workflow.toolset().to_hex(),
        script: workflow.script().to_hex(),
        input: workflow.schemas().input.clone(),
        output: workflow.schemas().output.clone(),
        signals: workflow.schemas().signals.clone(),
        labels: Labels::from(stored.labels.clone()),
        secrets: workflow
            .secrets()
            .iter()
            .map(|(server, secret)| (server.to_owned(), secret.to_string()))
            .collect(),
        created_at: timestamp(stored.created_at),
        declarations: rewind_api::toolsets::Declarations {
            dts: declaration
                .map(|declaration| declaration.typescript.clone())
                .unwrap_or_default(),
            prompt: declaration
                .map(|declaration| declaration.prompt.clone())
                .unwrap_or_default(),
        },
        parent: workflow.suite().parent.map(|parent| parent.to_hex()),
        tests: workflow
            .suite()
            .tests
            .iter()
            .map(|test| SuiteTest {
                name: test.name.clone(),
                digest: test.test.to_hex(),
            })
            .collect(),
        retired: workflow
            .suite()
            .retired
            .iter()
            .map(|retired| RetiredTest {
                name: retired.name.clone(),
                digest: retired.test.to_hex(),
                reason: retired.reason.clone(),
            })
            .collect(),
        updated: workflow
            .suite()
            .updated
            .iter()
            .map(|updated| UpdatedTest {
                name: updated.name.clone(),
                from: updated.from.to_hex(),
                to: updated.to.to_hex(),
                reason: updated.reason.clone(),
            })
            .collect(),
    }
}

/// The payload an event holds, if any (`rewind-server`'s `views::payload_of`).
pub fn payload_of(event: &Event) -> Option<&rewind_core::Payload> {
    match event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(&started.input),
        Event::Command(Command::ScheduleToolCall { args, .. }) => Some(args),
        Event::Command(Command::CompleteRun { output }) => Some(output),
        Event::Completion(Completion::ToolCallCompleted { result, .. }) => Some(result),
        Event::Completion(Completion::SignalReceived { payload, .. }) => Some(payload),
        _ => None,
    }
}

/// The servers of a toolset, with their tools counted (`rewind-server`'s `toolsets::servers`).
fn servers(toolset: &rewind_core::Toolset) -> Vec<rewind_api::toolsets::ServerSummary> {
    use rewind_core::{McpTransport, ToolRoute};
    toolset
        .servers()
        .iter()
        .map(|server| rewind_api::toolsets::ServerSummary {
            name: server.name.clone(),
            transport: match server.transport {
                McpTransport::Http { .. } => rewind_api::toolsets::McpTransport::Http,
                McpTransport::Stdio { .. } => rewind_api::toolsets::McpTransport::Stdio,
            },
            auth: rewind_api::toolsets::Auth::of_core(&server.auth),
            tools: u32::try_from(
                toolset
                    .tools()
                    .iter()
                    .filter(|tool| matches!(&tool.route, ToolRoute::Mcp { server: of } if *of == server.name))
                    .count(),
            )
            .unwrap_or(u32::MAX),
        })
        .collect()
}

/// A stored toolset, as `POST /v1/toolsets` shows it (`rewind-server`'s `toolsets::view`).
pub fn toolset(stored: &rewind_engine::ports::StoredToolset) -> rewind_api::toolsets::Toolset {
    use rewind_api::toolsets::{OnInterruption, Route, Tool, ToolPolicy};
    use rewind_core::{Interruption, ToolRoute};
    let toolset = &stored.toolset;
    rewind_api::toolsets::Toolset {
        id: toolset.digest().to_hex(),
        servers: servers(toolset),
        // The evals' world has MCP servers only.
        apis: Vec::new(),
        tools: toolset
            .tools()
            .iter()
            .map(|tool| Tool {
                name: tool.name.clone(),
                route: match tool.route {
                    ToolRoute::Mcp { .. } => Route::Mcp,
                    ToolRoute::Api { .. } => Route::Api,
                    ToolRoute::Host { .. } => Route::Host,
                    ToolRoute::Builtin(_) => Route::Builtin,
                },
                server: match &tool.route {
                    ToolRoute::Mcp { server } => Some(server.clone()),
                    _ => None,
                },
                api: match &tool.route {
                    ToolRoute::Api { api } => Some(api.clone()),
                    _ => None,
                },
                description: tool.description.clone().unwrap_or_default(),
                input_schema: tool.input_schema.clone(),
                output_schema: tool.output_schema.clone(),
                policy: ToolPolicy {
                    on_interruption: Some(match tool.policy.on_interruption {
                        Interruption::Retry => OnInterruption::Retry,
                        Interruption::AtMostOnce => OnInterruption::AtMostOnce,
                    }),
                    approval: Some(tool.policy.approval),
                    timeout: tool.policy.execution_timeout.map(Into::into),
                    heartbeat_timeout: Some(tool.policy.heartbeat_timeout.into()),
                    concurrency: Some(tool.policy.concurrency),
                },
            })
            .collect(),
        workflows: toolset
            .workflows()
            .iter()
            .map(|workflow| rewind_api::workflows::PinnedWorkflow {
                name: workflow.name.clone(),
                version: workflow.version.to_hex(),
                description: workflow.description.clone(),
            })
            .collect(),
        declarations: rewind_api::toolsets::Declarations {
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
            script_location("RangeError: invalid amount -5\n    at main (file:///script.ts:5:31)"),
            Some((5, 31))
        );
        assert_eq!(
            script_location("cannot import \"node:fs\" (script.ts:1:16): scripts can only…"),
            Some((1, 16))
        );
        assert_eq!(script_location("ToolError: crm.get_order failed"), None);
    }

    #[test]
    fn frames_mark_the_column_between_their_neighbors() {
        let source = "import { crm } from \"rewind:tools\";\n\nexport default async function main() {\n  const order = await crm.get_order({ id: \"O-504\" });\n  return order;\n}\n";
        assert_eq!(
            code_frame(source, 4, 23).unwrap(),
            "3 | export default async function main() {\n4 |   const order = await crm.get_order({ id: \"O-504\" });\n  |                       ^\n5 |   return order;"
        );
        assert_eq!(code_frame(source, 40, 1), None);
    }
}
