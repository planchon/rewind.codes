//! What the API shows of what the store holds: runs, journal entries, payloads, tasks. Every
//! view is a projection of the journal and of the run's record; nothing here writes.

use std::collections::{BTreeMap, BTreeSet};

use rewind_api::common::{AbortReason, Callsite, Labels, Payload, RunMode, RunStatus, timestamp};
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
use rewind_engine::ports::{
    BlobStore, JournalStore, ParkKind, PayloadRecord, QuarantineKind, ReadModel, RunRecord, RunRef,
    StoreError, TaskRef, TenantId,
};
use rewind_engine::retention::Housekeeper;

use crate::error::ApiError;
use crate::wiring::blobs::Blobs;
use crate::wiring::store::Store;

/// Log lines a run's state shows.
const LOG_LINES: usize = 20;

/// Reads what the views need.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Views<'a> {
    pub(crate) store: &'a Store,
    pub(crate) blobs: &'a Blobs,
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
        Event::Command(Command::ScheduleToolCall { args, .. }) => Some(args),
        Event::Command(Command::CompleteRun { output }) => Some(output),
        Event::Completion(Completion::ToolCallCompleted { result, .. }) => Some(result),
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

impl Views<'_> {
    fn housekeeper(&self) -> Housekeeper<'_, Store, Blobs, Store, Store> {
        Housekeeper {
            journal: self.store,
            blobs: self.blobs,
            read_model: self.store,
            secrets: self.store,
            batch: 100,
        }
    }

    /// The run's history: from the store, or from its archive once archived.
    pub(crate) async fn history(&self, record: &RunRecord) -> Result<Vec<HistoryEntry>, ApiError> {
        if record.archive.is_none() {
            match self.store.load_history(&record.run, 0).await {
                Ok(history) => return Ok(history),
                Err(StoreError::Archived(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let bytes = self
            .housekeeper()
            .load_archive(&record.run)
            .await?
            .ok_or_else(|| {
                ApiError::internal(format!("the archive of run {} is gone", record.run.id))
            })?;
        let text = String::from_utf8(bytes).map_err(ApiError::internal)?;
        let document = rewind_format::decode_journal(&text).map_err(ApiError::internal)?;
        Ok(document.entries)
    }

    /// The history from `from`, for the event stream.
    pub(crate) async fn history_from(
        &self,
        record: &RunRecord,
        from: u64,
    ) -> Result<Vec<HistoryEntry>, ApiError> {
        if record.archive.is_none() {
            match self.store.load_history(&record.run, from).await {
                Ok(history) => return Ok(history),
                Err(StoreError::Archived(_)) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut history = self.history(record).await?;
        history.retain(|entry| entry.index >= from);
        Ok(history)
    }

    /// The API views of payloads held by their digest, with their value when it is at most
    /// `limit` bytes.
    pub(crate) async fn payloads(
        &self,
        tenant: &TenantId,
        digests: impl IntoIterator<Item = Digest>,
        limit: u64,
    ) -> Result<BTreeMap<Digest, Payload>, ApiError> {
        let mut views = BTreeMap::new();
        for digest in digests.into_iter().collect::<BTreeSet<_>>() {
            let view = match self.store.get_payload(tenant, digest).await? {
                Some(record) => self.record_view(tenant, record, limit).await?,
                None => match self.blob_value(tenant, digest).await? {
                    Some(value) => Payload::of(value, limit),
                    None => Payload::reference(digest.to_hex(), 0, ""),
                },
            };
            views.insert(digest, view);
        }
        Ok(views)
    }

    async fn record_view(
        &self,
        tenant: &TenantId,
        record: PayloadRecord,
        limit: u64,
    ) -> Result<Payload, ApiError> {
        let mut view = Payload::reference(
            record.digest.to_hex(),
            record.size,
            rewind_api::common::preview(&record.preview),
        );
        if record.size <= limit {
            view.value = match record.value {
                Some(value) => Some(value),
                None => self.blob_value(tenant, record.digest).await?,
            };
        }
        Ok(view)
    }

    async fn blob_value(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<serde_json::Value>, ApiError> {
        let Some(bytes) = self.blobs.get(tenant, digest).await? else {
            return Ok(None);
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(ApiError::internal)
    }

    /// A payload's value, wherever it lives; `None` when it is nowhere.
    pub(crate) async fn value(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<Option<serde_json::Value>, ApiError> {
        if let Some(value) = self
            .store
            .get_payload(tenant, digest)
            .await?
            .and_then(|record| record.value)
        {
            return Ok(Some(value));
        }
        self.blob_value(tenant, digest).await
    }

    /// Puts back in `entries` the values of the payloads held by their digest: a
    /// self-contained history, for exports.
    pub(crate) async fn inline(
        &self,
        tenant: &TenantId,
        entries: &mut [HistoryEntry],
    ) -> Result<(), ApiError> {
        for entry in entries.iter_mut() {
            let payload = match &mut entry.event {
                Event::Lifecycle(Lifecycle::RunStarted(started)) => &mut started.input,
                Event::Command(Command::ScheduleToolCall { args, .. }) => args,
                Event::Command(Command::CompleteRun { output }) => output,
                Event::Completion(Completion::ToolCallCompleted { result, .. }) => result,
                Event::Completion(Completion::SignalReceived { payload, .. }) => payload,
                _ => continue,
            };
            if let rewind_core::Payload::Digest(digest) = payload {
                let digest = *digest;
                let value = self
                    .value(tenant, digest)
                    .await?
                    .ok_or_else(|| ApiError::internal(format!("payload {digest} is missing")))?;
                *payload = rewind_core::Payload::Value(value);
            }
        }
        Ok(())
    }

    /// The rows of the payloads `history` holds by digest: their size and preview.
    pub(crate) async fn payload_rows(
        &self,
        tenant: &TenantId,
        history: &[HistoryEntry],
    ) -> Result<BTreeMap<Digest, PayloadRecord>, ApiError> {
        let mut rows = BTreeMap::new();
        for entry in history {
            if let Some(rewind_core::Payload::Digest(digest)) = payload_of(&entry.event)
                && !rows.contains_key(digest)
                && let Some(row) = self.store.get_payload(tenant, *digest).await?
            {
                rows.insert(*digest, row);
            }
        }
        Ok(rows)
    }

    /// The API views of journal entries.
    pub(crate) async fn entries(
        &self,
        tenant: &TenantId,
        entries: &[HistoryEntry],
        limit: u64,
    ) -> Result<Vec<Entry>, ApiError> {
        let digests = entries
            .iter()
            .filter_map(|entry| match payload_of(&entry.event) {
                Some(rewind_core::Payload::Digest(digest)) => Some(*digest),
                _ => None,
            });
        let held = self.payloads(tenant, digests, limit).await?;
        Ok(entries
            .iter()
            .map(|entry| Entry::from_core(entry, |payload| view(payload, &held, limit)))
            .collect())
    }

    /// The state of a run.
    pub(crate) async fn run(&self, record: &RunRecord) -> Result<Run, ApiError> {
        let history = self.history(record).await?;
        self.run_with(record, &history).await
    }

    /// The state of a run whose history is at hand.
    pub(crate) async fn run_with(
        &self,
        record: &RunRecord,
        history: &[HistoryEntry],
    ) -> Result<Run, ApiError> {
        let limit = rewind_api::common::PAYLOAD_VALUE_LIMIT;
        let tenant = &record.run.tenant;
        let state = RunState::from_history(history).map_err(ApiError::internal)?;
        let status = if history.is_empty() {
            RunStatus::Pending
        } else {
            state.status().into()
        };
        let digests = history
            .iter()
            .filter_map(|entry| match payload_of(&entry.event) {
                Some(rewind_core::Payload::Digest(digest)) => Some(*digest),
                _ => None,
            });
        let held = self.payloads(tenant, digests, limit).await?;
        let payload = |payload: &rewind_core::Payload| view(payload, &held, limit);

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
                error = Some(self.run_error(record, history, failure, &payload).await?);
            }
            Some(RunOutcome::Aborted { reason }) => abort = Some(AbortReason::from(reason)),
            None => {}
        }
        let waiting_for = if state.is_terminal() || history.is_empty() {
            Vec::new()
        } else {
            self.waiting(record, history, &state, &payload).await?
        };
        let logs = logs(history);
        let attempts = self
            .store
            .attempts(&record.run)
            .await?
            .into_iter()
            .filter(|(_, attempts)| *attempts > 1)
            .map(|(seq, attempts)| CallAttempts {
                seq: seq.0,
                attempts,
            })
            .collect();
        let (trace, _) = rewind_engine::telemetry::trace_of(record);
        Ok(Run {
            id: record.run.id.to_string(),
            namespace: namespace(&record.namespace),
            status,
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
            // Only live runs are stored: mock runs live in memory (`POST /v1/test`).
            mode: started.map_or(RunMode::Live, |started| started.mode.into()),
            labels: Labels::from(record.metadata.labels.clone()),
            trace_id: trace.to_string(),
            parent: record.metadata.parent.map(|parent| ForkParent {
                run: parent.run.to_string(),
                at: parent.at,
            }),
            workflow: record.metadata.workflow.as_ref().map(|workflow| {
                rewind_api::workflows::RunWorkflow {
                    name: workflow.name.clone(),
                    version: workflow.version.to_hex(),
                }
            }),
            started_by: record
                .metadata
                .started_by
                .map(|by| rewind_api::workflows::StartedBy {
                    run: by.run.to_string(),
                    seq: by.seq.0,
                    detached: by.detached,
                }),
            input: started.map(|started| payload(&started.input)),
            output,
            error,
            abort,
            waiting_for,
            logs,
            attempts,
            history_len: (history.len() as u64).max(record.history_len),
            created_at: timestamp(record.created_at),
            updated_at: timestamp(record.updated_at),
            ended_at: record.ended_at.map(timestamp),
            expires_at: timestamp(record.expires_at),
        })
    }

    async fn run_error(
        &self,
        record: &RunRecord,
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
                .store
                .get_script(&record.run.tenant, record.script)
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

    async fn waiting(
        &self,
        record: &RunRecord,
        history: &[HistoryEntry],
        state: &RunState,
        payload: &impl Fn(&rewind_core::Payload) -> Payload,
    ) -> Result<Vec<Waiting>, ApiError> {
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
        let tasks: BTreeMap<Seq, _> = self
            .store
            .tool_tasks(&record.run)
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
                Some(rewind_engine::ports::TaskState::Deferred { .. }) => {
                    (TaskState::Deferred, None)
                }
                Some(rewind_engine::ports::TaskState::Parked(reason)) => (
                    TaskState::Parked,
                    Some(match reason.kind {
                        ParkKind::SchemaChanged => ParkedReason::SchemaChanged,
                        ParkKind::CredentialsExpired => ParkedReason::CredentialsExpired,
                    }),
                ),
            };
            waiting.push(Waiting::ToolCall {
                seq: seq.0,
                tool: tool.clone(),
                args: args.unwrap_or_else(|| Payload::reference("", 0, "")),
                task_id: task_id(&TaskRef {
                    run: record.run.clone(),
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
        Ok(waiting)
    }
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
