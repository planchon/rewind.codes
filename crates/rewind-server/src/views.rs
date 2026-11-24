//! What the API shows of a run: its state, its journal entries, its payloads, its tasks. Every
//! view is read from the run's owner (its head, summary, history and tasks, live or sealed) and
//! from the run's blobs; nothing here writes.

use std::collections::{BTreeMap, BTreeSet};

use rewind_api::common::{AbortReason, Callsite, Labels, Payload, RunMode, timestamp};
use rewind_api::events::Entry;
use rewind_api::runs::{
    CallAttempts, FailedCall, ForkParent, LogLine, Quarantine, QuarantineCause, Run, RunError,
    Waiting,
};
use rewind_api::tasks::{ParkedReason, TaskState};
use rewind_core::{
    Annotation, Command, Completion, Digest, Event, HistoryEntry, Lifecycle, RunOutcome, RunState,
    ScriptError, Seq,
};
use rewind_engine::failures::script_location;
use rewind_engine::ports::link::RunView;
use rewind_engine::ports::{
    BlobOwner, BlobStore, CatalogStore, ParkKind, QuarantineKind, RunRef, TaskRecord, TaskRef,
    TenantId,
};

use crate::app::App;
use crate::error::ApiError;

/// Log lines a run's state shows.
const LOG_LINES: usize = 20;

/// Reads what the views need, through the node of this process.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Views<'a> {
    pub(crate) app: &'a App,
}

/// A task's id in the API: `{run_id}.{seq}`.
pub(crate) fn task_id(task: &TaskRef) -> String {
    format!("{}.{}", task.run.id, task.seq.0)
}

/// Reads a task id of the API.
pub(crate) fn parse_task_id(tenant: &TenantId, id: &str) -> Option<TaskRef> {
    let (run, seq) = id.rsplit_once('.')?;
    Some(TaskRef {
        run: RunRef::new(tenant.clone(), run.parse().ok()?),
        seq: Seq(seq.parse().ok()?),
    })
}

/// The payload an event carries, if any.
pub(crate) fn payload_of(event: &Event) -> Option<&rewind_core::Payload> {
    match event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(&started.input),
        Event::Command(Command::ScheduleCall { args, .. }) => Some(args),
        Event::Command(Command::CompleteRun { output }) => Some(output),
        Event::Completion(Completion::CallCompleted { result, .. }) => Some(result),
        Event::Completion(Completion::SignalReceived { payload, .. }) => Some(payload),
        _ => None,
    }
}

/// A few lines of the source around a position, the column marked.
pub(crate) fn code_frame(source: &str, line: u32, column: u32) -> Option<String> {
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

/// The digests of the payloads the entries hold by digest: those above 64 KiB, in the run's
/// blobs.
fn held_by_digest(entries: &[HistoryEntry]) -> BTreeSet<Digest> {
    entries
        .iter()
        .filter_map(|entry| match payload_of(&entry.event) {
            Some(rewind_core::Payload::Digest(digest)) => Some(*digest),
            _ => None,
        })
        .collect()
}

impl Views<'_> {
    /// The run's history from `from`, every payload up to 64 KiB with its value: from its
    /// partition, or from its sealed journal.
    pub(crate) async fn history_from(
        &self,
        run: &RunRef,
        from: u64,
    ) -> Result<Vec<HistoryEntry>, ApiError> {
        self.app
            .edge
            .history(&mut self.app.gateway(), run, from, true)
            .await?
            .ok_or_else(|| ApiError::run_not_found(&run.id.to_string()))
    }

    /// The run's whole history.
    pub(crate) async fn history(&self, run: &RunRef) -> Result<Vec<HistoryEntry>, ApiError> {
        self.history_from(run, 0).await
    }

    /// The run's call tasks, by seq: what its owner records, the calls in flight `claimed`.
    pub(crate) async fn tasks(&self, run: &RunRef) -> Result<Vec<TaskRecord>, ApiError> {
        Ok(self
            .app
            .edge
            .task_records(&mut self.app.gateway(), run)
            .await?)
    }

    /// The views of the payloads above 64 KiB, read from the run's blobs, with their value when
    /// it is at most `limit` bytes.
    async fn blob_payloads(
        &self,
        run: &RunRef,
        digests: BTreeSet<Digest>,
        limit: u64,
    ) -> Result<BTreeMap<Digest, Payload>, ApiError> {
        let mut views = BTreeMap::new();
        for digest in digests {
            let view = match self.blob(run, digest).await? {
                Some(bytes) => {
                    let size = bytes.len() as u64;
                    let text = String::from_utf8_lossy(&bytes);
                    let mut view = Payload::reference(
                        digest.to_hex(),
                        size,
                        rewind_api::common::preview(&text),
                    );
                    if size <= limit {
                        view.value =
                            Some(serde_json::from_slice(&bytes).map_err(ApiError::internal)?);
                    }
                    view
                }
                None => Payload::reference(digest.to_hex(), 0, ""),
            };
            views.insert(digest, view);
        }
        Ok(views)
    }

    async fn blob(&self, run: &RunRef, digest: Digest) -> Result<Option<Vec<u8>>, ApiError> {
        Ok(self
            .app
            .blobs()
            .get(&BlobOwner::Run(run.clone()), digest)
            .await?)
    }

    /// A payload's value, through a run that holds it; `None` when the run does not.
    pub(crate) async fn value(
        &self,
        run: &RunRef,
        digest: Digest,
    ) -> Result<Option<serde_json::Value>, ApiError> {
        let history = self.history(run).await?;
        for entry in &history {
            match payload_of(&entry.event) {
                Some(rewind_core::Payload::Value(value))
                    if rewind_core::Payload::digest_of(value) == digest =>
                {
                    return Ok(Some(value.clone()));
                }
                Some(rewind_core::Payload::Digest(held)) if *held == digest => {
                    return match self.blob(run, digest).await? {
                        Some(bytes) => serde_json::from_slice(&bytes)
                            .map(Some)
                            .map_err(ApiError::internal),
                        None => Ok(None),
                    };
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// The API views of journal entries of a run.
    pub(crate) async fn entries(
        &self,
        run: &RunRef,
        entries: &[HistoryEntry],
        limit: u64,
    ) -> Result<Vec<Entry>, ApiError> {
        let held = self
            .blob_payloads(run, held_by_digest(entries), limit)
            .await?;
        Ok(entries
            .iter()
            .map(|entry| Entry::from_core(entry, |payload| view(payload, &held, limit)))
            .collect())
    }

    /// The state of a run.
    pub(crate) async fn run(&self, run: &RunView) -> Result<Run, ApiError> {
        let history = self.history(&run.head.run).await?;
        let tasks = if run.head.is_ended() {
            Vec::new()
        } else {
            self.tasks(&run.head.run).await?
        };
        self.run_with(run, &history, &tasks).await
    }

    /// The state of a run whose history and tasks are at hand.
    pub(crate) async fn run_with(
        &self,
        view: &RunView,
        history: &[HistoryEntry],
        tasks: &[TaskRecord],
    ) -> Result<Run, ApiError> {
        let head = &view.head;
        let limit = rewind_api::common::PAYLOAD_VALUE_LIMIT;
        let state = RunState::from_history(history).map_err(ApiError::internal)?;
        let held = self
            .blob_payloads(&head.run, held_by_digest(history), limit)
            .await?;
        let payload = |payload: &rewind_core::Payload| view_of(payload, &held, limit);

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
                error = Some(self.run_error(head, history, failure, &payload).await?);
            }
            Some(RunOutcome::Aborted { reason }) => abort = Some(AbortReason::from(reason)),
            None => {}
        }
        let waiting_for = if state.is_terminal() || history.is_empty() {
            Vec::new()
        } else {
            waiting(&head.run, history, &state, tasks, &payload)
        };
        let attempts = tasks
            .iter()
            .filter(|record| record.task.attempt > 1)
            .map(|record| CallAttempts {
                seq: record.task.task.seq.0,
                attempts: record.task.attempt,
            })
            .collect();
        let tenant = &head.run.tenant;
        let script_title = self
            .app
            .titles(tenant, [head.script])
            .await?
            .remove(&head.script)
            .map(|title| title.title);
        // A run named by a handle, or by its id once it is erased.
        let named = |handle: Option<&rewind_core::RunHandle>, run: rewind_core::RunId| {
            handle.map_or_else(|| run.to_string(), ToString::to_string)
        };
        let workflow = match &head.metadata.workflow {
            Some(workflow) => Some(rewind_api::workflows::RunWorkflow {
                name: workflow.name.clone(),
                version: workflow.version.to_hex(),
                // A version is never erased: 0 would say its row is gone.
                number: self
                    .app
                    .catalog()
                    .get_workflow(tenant, workflow.version)
                    .await?
                    .map_or(0, |stored| stored.number),
            }),
            None => None,
        };
        Ok(Run {
            id: head.run.id.to_string(),
            handle: head.handle().to_string(),
            namespace: namespace(&head.namespace),
            status: head.status.into(),
            quarantine: head.quarantine.as_ref().map(|quarantine| Quarantine {
                cause: match quarantine.kind {
                    QuarantineKind::NonDeterminism => QuarantineCause::NonDeterminism,
                    QuarantineKind::Failures => QuarantineCause::RepeatedFailures,
                },
                reason: quarantine.reason.clone(),
            }),
            script: head.script.to_hex(),
            script_title,
            action_set: head.action_set.to_hex(),
            epoch: head.epoch.0,
            // Only live runs are stored: mock runs live in memory (`POST /v1/test`).
            mode: started.map_or(RunMode::Live, |started| started.mode.into()),
            labels: Labels::from(head.metadata.labels.clone()),
            secrets: head
                .metadata
                .secrets
                .iter()
                .map(|(server, secret)| (server.to_owned(), secret.to_string()))
                .collect(),
            trace_id: trace_id(head),
            parent: head.metadata.parent.map(|parent| ForkParent {
                run: parent.run.to_string(),
                handle: named(view.parent.as_ref(), parent.run),
                at: parent.at,
            }),
            workflow,
            started_by: head
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
            logs: logs(history),
            attempts,
            history_len: (history.len() as u64).max(head.history_len),
            created_at: timestamp(head.created_at),
            updated_at: timestamp(head.updated_at),
            ended_at: head.ended_at.map(timestamp),
            expires_at: timestamp(head.expires_at),
        })
    }

    async fn run_error(
        &self,
        head: &rewind_engine::ports::RunHead,
        history: &[HistoryEntry],
        failure: &ScriptError,
        payload: &impl Fn(&rewind_core::Payload) -> Payload,
    ) -> Result<RunError, ApiError> {
        let location = failure
            .stack
            .as_deref()
            .and_then(script_location)
            .or_else(|| script_location(&failure.message));
        let frame = match location {
            Some((line, column)) => self
                .app
                .catalog()
                .get_script(&head.run.tenant, head.script)
                .await?
                .and_then(|stored| code_frame(stored.script.source(), line, column)),
            None => None,
        };
        let failed_call = failure
            .failed_call(history)
            .and_then(|seq| failed_call(history, seq, payload));
        Ok(RunError {
            name: failure.name.clone(),
            message: failure.message.clone(),
            stack: failure.stack.clone(),
            location: location.map(|(line, column)| Callsite { line, column }),
            frame,
            failed_call,
        })
    }
}

/// The run's trace id: the harness's when it gave a valid `traceparent`, the run's id otherwise.
pub(crate) fn trace_id(head: &rewind_engine::ports::RunHead) -> String {
    head.metadata
        .trace_context
        .as_ref()
        .and_then(|context| {
            let trace = context.traceparent.split('-').nth(1)?;
            (trace.len() == 32 && trace != "0".repeat(32)).then(|| trace.to_owned())
        })
        .unwrap_or_else(|| head.run.id.as_uuid().simple().to_string())
}

/// What a run that has not ended waits for.
fn waiting(
    run: &RunRef,
    history: &[HistoryEntry],
    state: &RunState,
    tasks: &[TaskRecord],
    payload: &impl Fn(&rewind_core::Payload) -> Payload,
) -> Vec<Waiting> {
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
            Event::Command(Command::StartTimer {
                seq,
                fire_at,
                callsite,
                ..
            }) => {
                timers.insert(*seq, (*fire_at, callsite.map(Callsite::from)));
            }
            _ => {}
        }
    }
    let tasks: BTreeMap<Seq, _> = tasks
        .iter()
        .map(|record| (record.task.task.seq, &record.task))
        .collect();
    let mut waiting = Vec::new();
    for (seq, action) in state.pending_calls() {
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
        waiting.push(Waiting::Call {
            seq: seq.0,
            action: action.clone(),
            args: args.unwrap_or_else(|| Payload::reference("", 0, "")),
            task_id: task_id(&TaskRef {
                run: run.clone(),
                seq: *seq,
            }),
            state: task_state,
            parked,
            attempt: task.map_or(1, |task| task.attempt.max(1)),
            callsite,
        });
    }
    let timeouts: BTreeSet<Seq> = state
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
            callsite: timers.get(seq).and_then(|(_, callsite)| *callsite),
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
    waiting
}

/// The view of a payload, from the values loaded in `held` for those held by digest.
fn view_of(
    payload: &rewind_core::Payload,
    held: &BTreeMap<Digest, Payload>,
    limit: u64,
) -> Payload {
    view(payload, held, limit)
}

/// The view of a payload, from the values loaded in `held` for those held by digest.
pub(crate) fn view(
    payload: &rewind_core::Payload,
    held: &BTreeMap<Digest, Payload>,
    limit: u64,
) -> Payload {
    match payload {
        rewind_core::Payload::Value(value) => Payload::of(value.clone(), limit),
        rewind_core::Payload::Digest(digest) => held
            .get(digest)
            .cloned()
            .unwrap_or_else(|| Payload::reference(digest.to_hex(), 0, "")),
    }
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
                level: (*level).into(),
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

/// The call whose failure the script did not catch: its `seq`, as `ScriptError::failed_call`
/// finds it, with its arguments and recorded failure.
fn failed_call(
    history: &[HistoryEntry],
    seq: rewind_core::Seq,
    payload: &impl Fn(&rewind_core::Payload) -> Payload,
) -> Option<FailedCall> {
    let error = history.iter().find_map(|entry| match &entry.event {
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
            error: error.into(),
            callsite: callsite.map(Callsite::from),
        }),
        _ => None,
    })
}

/// A namespace as the API names it: the engine and the API check the same rule.
pub(crate) fn namespace(
    name: &rewind_engine::ports::NamespaceName,
) -> rewind_api::namespaces::NamespaceName {
    rewind_api::namespaces::NamespaceName::new(name.as_str())
        .expect("the engine and the API check the same rule for namespaces")
}

/// A namespace as the engine names it: the engine and the API check the same rule.
pub(crate) fn engine_namespace(
    name: &rewind_api::namespaces::NamespaceName,
) -> rewind_engine::ports::NamespaceName {
    rewind_engine::ports::NamespaceName::new(name.as_str())
        .expect("the engine and the API check the same rule for namespaces")
}

/// Limits as the API writes them.
pub(crate) fn quotas(quotas: &rewind_engine::ports::Quotas) -> rewind_api::namespaces::Quotas {
    rewind_api::namespaces::Quotas {
        cpu_per_month: quotas.cpu_per_month.map(Into::into),
        active_runs: quotas.active_runs,
        runs_per_month: quotas.runs_per_month,
        scripts: quotas.scripts,
        max_ttl: quotas.max_ttl.map(Into::into),
        max_script_lines: quotas.max_script_lines,
    }
}

/// Limits as the engine reads them.
pub(crate) fn engine_quotas(
    quotas: &rewind_api::namespaces::Quotas,
) -> rewind_engine::ports::Quotas {
    rewind_engine::ports::Quotas {
        cpu_per_month: quotas.cpu_per_month.map(|duration| duration.get()),
        active_runs: quotas.active_runs,
        runs_per_month: quotas.runs_per_month,
        scripts: quotas.scripts,
        max_ttl: quotas.max_ttl.map(|duration| duration.get()),
        max_script_lines: quotas.max_script_lines,
    }
}

/// A month's use as the API writes it.
pub(crate) fn usage(usage: &rewind_engine::ports::Usage) -> rewind_api::namespaces::QuotaUsage {
    rewind_api::namespaces::QuotaUsage {
        cpu: usage.cpu.into(),
        active_runs: usage.active_runs,
        runs_started: usage.runs_started,
        scripts: usage.scripts,
    }
}

/// A quota alert as the API and the webhook `quota.alert` write it.
pub(crate) fn quota_alert(
    alert: &rewind_engine::ports::QuotaAlert,
) -> rewind_api::namespaces::QuotaAlert {
    use rewind_api::namespaces::QuotaKind;
    use rewind_engine::ports::QuotaKind as Kind;
    rewind_api::namespaces::QuotaAlert {
        namespace: alert.key.namespace.as_ref().map(namespace),
        quota: match alert.key.quota {
            Kind::CpuPerMonth => QuotaKind::CpuPerMonth,
            Kind::ActiveRuns => QuotaKind::ActiveRuns,
            Kind::RunsPerMonth => QuotaKind::RunsPerMonth,
            Kind::Scripts => QuotaKind::Scripts,
            Kind::MaxTtl => QuotaKind::MaxTtl,
            Kind::MaxScriptLines => QuotaKind::MaxScriptLines,
        },
        threshold: alert.key.threshold.percent(),
        used: alert.used,
        limit: alert.limit,
        period: alert.key.period.to_string(),
        raised_at: rewind_api::common::timestamp(alert.raised_at),
        sent_at: alert.sent_at.map(rewind_api::common::timestamp),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_marks_the_column() {
        let source = "const a = 1;\nconst b = a.c.d;\n\nexport default b;";
        assert_eq!(
            code_frame(source, 2, 13),
            Some("1 | const a = 1;\n2 | const b = a.c.d;\n  |             ^".into())
        );
        assert_eq!(code_frame(source, 9, 1), None);
    }

    #[test]
    fn task_ids_name_the_run_and_the_call() {
        let tenant = TenantId::local();
        let task = TaskRef {
            run: RunRef::new(tenant.clone(), rewind_core::RunId::from_u128(7)),
            seq: Seq(3),
        };
        let id = task_id(&task);
        assert_eq!(parse_task_id(&tenant, &id), Some(task));
        assert_eq!(parse_task_id(&tenant, "nope"), None);
    }
}
