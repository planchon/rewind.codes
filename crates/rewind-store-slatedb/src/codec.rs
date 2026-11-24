//! The engine's types to and from the partition records of `rewind-format`
//! (`archi/07-code-architecture.md`, principle 4: each boundary owns its representation).
//!
//! Writing never fails; reading a record that names something the engine refuses (a tenant, a
//! namespace, an event that is no trigger) is [`PartitionError::Corrupt`].
//!
//! The conversions are public: `rewind-server`, a composition root, writes the records the
//! stored format already defines (a run head, an event, a summary, …) in the DTOs of its
//! node link rather than defining them a second time. `*_record` writes a record, `*_of` reads
//! one back.

use std::collections::BTreeMap;
use std::time::Duration;

use rewind_core::{Event, Lifecycle, Seq, Timestamp};
use rewind_engine::ports::{
    ActionCounts, Admission, Attempt, AttributeValue, CallTask, Counters, CredentialsExpiredReason,
    DueTimer, Ending, FailureCounts, ForkSource, Gauges, InboxItem, IndexedSummary, LatestRun,
    LogRecord, Month, NamespaceName, OrganizationState, OutboxEntry, OutboxId, ParkKind,
    ParkReason, PartitionError, PayloadRecord, Projection, Quarantine, QuarantineKind,
    QuarantineNote, QuotaAlert, QuotaAlertKey, QuotaKind, RunCounts, RunHead, RunMetadata, RunRef,
    RunSummary, RunWorkflow, ScriptCounts, SignalUrlTarget, Sketch, Span, SpanId, SpanKind,
    SpanStatus, StartedBy, TaskPhase, TaskRecord, TaskRef, TaskState, TenantId, Threshold,
    TimerKind, TraceContext, TraceId, Trigger, Wait, WaitKind, WaitingFor, Watcher, WebhookEvent,
    WebhookEventKind,
};
use rewind_format::partition as record;

pub(crate) fn corrupt(message: impl Into<String>) -> PartitionError {
    PartitionError::Corrupt(message.into())
}

pub(crate) fn tenant(text: String) -> Result<TenantId, PartitionError> {
    TenantId::new(text).map_err(|err| corrupt(err.to_string()))
}

pub(crate) fn namespace(text: String) -> Result<NamespaceName, PartitionError> {
    NamespaceName::stored(text).map_err(|err| corrupt(err.to_string()))
}

/// Encodes a record.
pub(crate) fn encode<R: record::Record>(value: &R) -> Vec<u8> {
    record::encode(value)
}

/// Decodes a record.
pub(crate) fn decode<R: record::Record>(bytes: &[u8]) -> Result<R, PartitionError> {
    record::decode(bytes).map_err(|err| corrupt(err.to_string()))
}

// -------------------------------------------------------------------------------------------
// Run heads

pub fn head_record(head: &RunHead) -> record::RunHead {
    record::RunHead {
        run: head.run.id,
        tenant: head.run.tenant.to_string(),
        namespace: head.namespace.to_string(),
        origin: head.origin.clone(),
        suffix_len: head.suffix_len,
        code: head.code,
        action_set: head.action_set,
        status: head.status.into(),
        epoch: head.epoch.0,
        attempt: head.attempt.0,
        sealed: head.sealed.map(|attempt| attempt.0),
        failures: head.failures,
        attempts: head.attempts,
        quarantine: head.quarantine.as_ref().map(quarantine_record),
        waits: head.waits.iter().map(wait_record).collect(),
        expires_at: head.expires_at.0,
        history_len: head.history_len,
        metadata: metadata_record(&head.metadata),
        idempotency_key: head.idempotency_key.clone(),
        created_at: head.created_at.0,
        updated_at: head.updated_at.0,
        ended_at: head.ended_at.map(|at| at.0),
    }
}

pub fn head_of(record: record::RunHead) -> Result<RunHead, PartitionError> {
    Ok(RunHead {
        run: RunRef::new(tenant(record.tenant)?, record.run),
        namespace: namespace(record.namespace)?,
        origin: record.origin,
        suffix_len: record.suffix_len,
        code: record.code,
        action_set: record.action_set,
        status: record.status.into(),
        epoch: rewind_core::Epoch(record.epoch),
        attempt: Attempt(record.attempt),
        sealed: record.sealed.map(Attempt),
        failures: record.failures,
        attempts: record.attempts,
        quarantine: record.quarantine.map(quarantine_of),
        waits: record.waits.into_iter().map(wait_of).collect(),
        expires_at: Timestamp(record.expires_at),
        history_len: record.history_len,
        metadata: metadata_of(record.metadata),
        idempotency_key: record.idempotency_key,
        created_at: Timestamp(record.created_at),
        updated_at: Timestamp(record.updated_at),
        ended_at: record.ended_at.map(Timestamp),
    })
}

pub fn metadata_record(metadata: &RunMetadata) -> record::Metadata {
    record::Metadata {
        labels: metadata.labels.clone(),
        trace_context: metadata
            .trace_context
            .as_ref()
            .map(|context| record::TraceContext {
                traceparent: context.traceparent.clone(),
                tracestate: context.tracestate.clone(),
            }),
        source: metadata.source.map(|parent| record::ForkSource {
            run: parent.run,
            at: parent.at,
        }),
        workflow: metadata
            .workflow
            .as_ref()
            .map(|workflow| record::RunWorkflow {
                name: workflow.name.clone(),
                version: workflow.version,
            }),
        started_by: metadata.started_by.map(|started| record::StartedBy {
            run: started.run,
            seq: started.seq.0,
            detached: started.detached,
        }),
        scope: metadata.scope.clone(),
        secrets: metadata.secrets.clone(),
    }
}

pub fn metadata_of(record: record::Metadata) -> RunMetadata {
    RunMetadata {
        labels: record.labels,
        trace_context: record.trace_context.map(|context| TraceContext {
            traceparent: context.traceparent,
            tracestate: context.tracestate,
        }),
        source: record.source.map(|parent| ForkSource {
            run: parent.run,
            at: parent.at,
        }),
        workflow: record.workflow.map(|workflow| RunWorkflow {
            name: workflow.name,
            version: workflow.version,
        }),
        started_by: record.started_by.map(|started| StartedBy {
            run: started.run,
            seq: Seq(started.seq),
            detached: started.detached,
        }),
        scope: record.scope,
        secrets: record.secrets,
    }
}

pub fn quarantine_kind_record(kind: QuarantineKind) -> record::QuarantineKind {
    match kind {
        QuarantineKind::NonDeterminism => record::QuarantineKind::NonDeterminism,
        QuarantineKind::Failures => record::QuarantineKind::Failures,
    }
}

pub fn quarantine_kind_of(kind: record::QuarantineKind) -> QuarantineKind {
    match kind {
        record::QuarantineKind::NonDeterminism => QuarantineKind::NonDeterminism,
        record::QuarantineKind::Failures => QuarantineKind::Failures,
    }
}

fn quarantine_record(quarantine: &Quarantine) -> record::Quarantine {
    record::Quarantine {
        kind: quarantine_kind_record(quarantine.kind),
        reason: quarantine.reason.clone(),
        since: quarantine.since.0,
        retried_by: quarantine.retried_by.clone(),
    }
}

fn quarantine_of(record: record::Quarantine) -> Quarantine {
    Quarantine {
        kind: quarantine_kind_of(record.kind),
        reason: record.reason,
        since: Timestamp(record.since),
        retried_by: record.retried_by,
    }
}

fn wait_record(wait: &Wait) -> record::Wait {
    record::Wait {
        kind: match wait.kind {
            WaitKind::Signal => record::WaitKind::Signal,
            WaitKind::Timer => record::WaitKind::Timer,
            WaitKind::Call => record::WaitKind::Call,
            WaitKind::Harness => record::WaitKind::Harness,
        },
        seq: wait.seq.map(|seq| seq.0),
        name: wait.name.clone(),
        since: wait.since.0,
        until: wait.until.map(|at| at.0),
        callsite: wait.callsite.map(|callsite| record::Callsite {
            line: callsite.line,
            column: callsite.column,
        }),
        task: wait.task.map(|phase| match phase {
            TaskPhase::Approval => record::TaskPhase::Approval,
            TaskPhase::Ready => record::TaskPhase::Ready,
            TaskPhase::Claimed => record::TaskPhase::Claimed,
            TaskPhase::Deferred => record::TaskPhase::Deferred,
            TaskPhase::Parked => record::TaskPhase::Parked,
        }),
    }
}

fn wait_of(record: record::Wait) -> Wait {
    Wait {
        kind: match record.kind {
            record::WaitKind::Signal => WaitKind::Signal,
            record::WaitKind::Timer => WaitKind::Timer,
            record::WaitKind::Call => WaitKind::Call,
            record::WaitKind::Harness => WaitKind::Harness,
        },
        seq: record.seq.map(Seq),
        name: record.name,
        since: Timestamp(record.since),
        until: record.until.map(Timestamp),
        callsite: record.callsite.map(|callsite| rewind_core::Callsite {
            line: callsite.line,
            column: callsite.column,
        }),
        task: record.task.map(|phase| match phase {
            record::TaskPhase::Approval => TaskPhase::Approval,
            record::TaskPhase::Ready => TaskPhase::Ready,
            record::TaskPhase::Claimed => TaskPhase::Claimed,
            record::TaskPhase::Deferred => TaskPhase::Deferred,
            record::TaskPhase::Parked => TaskPhase::Parked,
        }),
    }
}

// -------------------------------------------------------------------------------------------
// History, inbox, tasks, timers, payloads, watchers, signal URL tokens

pub fn event_record(entry: &rewind_core::HistoryEvent) -> record::HistoryEvent {
    record::HistoryEvent {
        index: entry.index,
        recorded_at: entry.recorded_at.0,
        event: entry.event.clone(),
    }
}

pub fn event_of(record: record::HistoryEvent) -> rewind_core::HistoryEvent {
    rewind_core::HistoryEvent {
        index: record.index,
        recorded_at: Timestamp(record.recorded_at),
        event: record.event,
    }
}

pub fn inbox_record(item: &InboxItem) -> record::InboxItem {
    record::InboxItem {
        id: item.id,
        received_at: item.received_at.0,
        trigger: match &item.trigger {
            Trigger::RunStarted(started) => {
                Event::Lifecycle(Lifecycle::RunStarted(started.clone()))
            }
            Trigger::Completion(completion) => Event::Completion(completion.clone()),
        },
    }
}

pub fn inbox_of(record: record::InboxItem) -> Result<InboxItem, PartitionError> {
    let trigger = match record.trigger {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Trigger::RunStarted(started),
        Event::Completion(completion) => Trigger::Completion(completion),
        other => {
            return Err(corrupt(format!(
                "inbox item {} holds an event that wakes no run: {other:?}",
                record.id
            )));
        }
    };
    Ok(InboxItem {
        id: record.id,
        trigger,
        received_at: Timestamp(record.received_at),
    })
}

pub fn task_record(task: &TaskRecord) -> record::Task {
    let action = &task.task;
    record::Task {
        tenant: action.task.run.tenant.to_string(),
        run: action.task.run.id,
        seq: action.task.seq.0,
        action_set: action.action_set,
        action: action.action.clone(),
        route: action.route.clone(),
        args: action.args.clone(),
        policy: action.policy,
        state: match &action.state {
            TaskState::Approval => record::TaskState::Approval {},
            TaskState::Ready { next_delivery_at } => record::TaskState::Ready {
                next_delivery_at: next_delivery_at.0,
            },
            TaskState::Claimed { owner, until } => record::TaskState::Claimed {
                owner: owner.clone(),
                until: until.0,
            },
            TaskState::Deferred { until } => record::TaskState::Deferred { until: until.0 },
            TaskState::Parked(reason) => record::TaskState::Parked {
                kind: match reason.kind {
                    ParkKind::SchemaChanged => record::ParkKind::SchemaChanged,
                    ParkKind::CredentialsRefused => record::ParkKind::CredentialsRefused,
                    ParkKind::SecretMissing => record::ParkKind::SecretMissing,
                },
                message: reason.message.clone(),
            },
        },
        delivery: action.delivery,
        schema_change_approved: action.schema_change_approved,
        created_at: action.created_at.0,
        token_hash: task.token_hash,
    }
}

pub fn task_of(record: record::Task) -> Result<TaskRecord, PartitionError> {
    Ok(TaskRecord {
        task: CallTask {
            task: TaskRef {
                run: RunRef::new(tenant(record.tenant)?, record.run),
                seq: Seq(record.seq),
            },
            action_set: record.action_set,
            action: record.action,
            route: record.route,
            args: record.args,
            policy: record.policy,
            state: match record.state {
                record::TaskState::Approval {} => TaskState::Approval,
                record::TaskState::Ready { next_delivery_at } => TaskState::Ready {
                    next_delivery_at: Timestamp(next_delivery_at),
                },
                record::TaskState::Claimed { owner, until } => TaskState::Claimed {
                    owner,
                    until: Timestamp(until),
                },
                record::TaskState::Deferred { until } => TaskState::Deferred {
                    until: Timestamp(until),
                },
                record::TaskState::Parked { kind, message } => TaskState::Parked(ParkReason {
                    kind: match kind {
                        record::ParkKind::SchemaChanged => ParkKind::SchemaChanged,
                        record::ParkKind::CredentialsRefused => ParkKind::CredentialsRefused,
                        record::ParkKind::SecretMissing => ParkKind::SecretMissing,
                    },
                    message,
                }),
            },
            delivery: record.delivery,
            schema_change_approved: record.schema_change_approved,
            created_at: Timestamp(record.created_at),
        },
        token_hash: record.token_hash,
    })
}

pub fn timer_record(timer: &DueTimer) -> record::Timer {
    record::Timer {
        at: timer.at.0,
        tenant: timer.kind.tenant().to_string(),
        kind: timer_kind_record(&timer.kind),
    }
}

/// A timer's kind, without its tenant: [`TimerKind::tenant`] is written beside it.
pub fn timer_kind_record(kind: &TimerKind) -> record::TimerKind {
    match kind {
        TimerKind::Code { run, seq } => record::TimerKind::Code {
            run: run.id,
            seq: seq.0,
        },
        TimerKind::CallTimeout { run, seq } => record::TimerKind::CallTimeout {
            run: run.id,
            seq: seq.0,
        },
        TimerKind::DeferredDeadline { run, seq } => record::TimerKind::DeferredDeadline {
            run: run.id,
            seq: seq.0,
        },
        TimerKind::RedeliveryBackoff { run, seq } => record::TimerKind::RedeliveryBackoff {
            run: run.id,
            seq: seq.0,
        },
        TimerKind::RunDeadline { run } => record::TimerKind::RunDeadline { run: run.id },
        TimerKind::ActivationRetry { run } => record::TimerKind::ActivationRetry { run: run.id },
        TimerKind::RetentionEnd { run } => record::TimerKind::RetentionEnd { run: run.id },
        TimerKind::WebhookRetry { entry, .. } => record::TimerKind::WebhookRetry { entry: entry.0 },
    }
}

pub fn timer_of(record: record::Timer) -> Result<DueTimer, PartitionError> {
    Ok(DueTimer {
        at: Timestamp(record.at),
        kind: timer_kind_of(tenant(record.tenant)?, record.kind),
    })
}

/// A timer's kind, of `tenant`.
pub fn timer_kind_of(tenant: TenantId, kind: record::TimerKind) -> TimerKind {
    let run = |id| RunRef::new(tenant.clone(), id);
    match kind {
        record::TimerKind::Code { run: id, seq } => TimerKind::Code {
            run: run(id),
            seq: Seq(seq),
        },
        record::TimerKind::CallTimeout { run: id, seq } => TimerKind::CallTimeout {
            run: run(id),
            seq: Seq(seq),
        },
        record::TimerKind::DeferredDeadline { run: id, seq } => TimerKind::DeferredDeadline {
            run: run(id),
            seq: Seq(seq),
        },
        record::TimerKind::RedeliveryBackoff { run: id, seq } => TimerKind::RedeliveryBackoff {
            run: run(id),
            seq: Seq(seq),
        },
        record::TimerKind::RunDeadline { run: id } => TimerKind::RunDeadline { run: run(id) },
        record::TimerKind::ActivationRetry { run: id } => {
            TimerKind::ActivationRetry { run: run(id) }
        }
        record::TimerKind::RetentionEnd { run: id } => TimerKind::RetentionEnd { run: run(id) },
        record::TimerKind::WebhookRetry { entry } => TimerKind::WebhookRetry {
            tenant: tenant.clone(),
            entry: OutboxId(entry),
        },
    }
}

pub fn payload_record(payload: &PayloadRecord) -> record::PayloadValue {
    record::PayloadValue {
        digest: payload.digest,
        size: payload.size,
        preview: payload.preview.clone(),
        value: payload.value.clone(),
    }
}

pub fn payload_of(record: record::PayloadValue) -> PayloadRecord {
    PayloadRecord {
        digest: record.digest,
        size: record.size,
        preview: record.preview,
        value: record.value,
    }
}

pub fn watcher_record(watcher: &Watcher) -> record::Watcher {
    record::Watcher {
        run: watcher.run,
        seq: watcher.seq.0,
        attached: watcher.attached,
    }
}

pub fn watcher_of(record: record::Watcher) -> Watcher {
    Watcher {
        run: record.run,
        seq: Seq(record.seq),
        attached: record.attached,
    }
}

pub fn signal_url_record(run: &RunRef, name: &str) -> record::SignalUrlToken {
    record::SignalUrlToken {
        tenant: run.tenant.to_string(),
        run: run.id,
        name: name.to_owned(),
    }
}

pub fn signal_url_of(record: record::SignalUrlToken) -> Result<SignalUrlTarget, PartitionError> {
    Ok(SignalUrlTarget {
        run: RunRef::new(tenant(record.tenant)?, record.run),
        name: record.name,
    })
}

// -------------------------------------------------------------------------------------------
// Summaries

pub fn summary_record(indexed: &IndexedSummary) -> record::Summary {
    let summary = &indexed.summary;
    record::Summary {
        tenant: summary.run.tenant.to_string(),
        run: summary.run.id,
        namespace: summary.namespace.to_string(),
        origin: summary.origin.clone(),
        suffix_len: summary.suffix_len,
        code: summary.code,
        headline: summary.headline.clone(),
        action_set: summary.action_set,
        status: summary.status.into(),
        mode: summary.mode.map(Into::into),
        labels: summary.labels.clone(),
        source: summary.source,
        workflow: summary.workflow.clone(),
        started_by: summary.started_by,
        started_at: summary.started_at.map(|at| at.0),
        ended_at: summary.ended_at.map(|at| at.0),
        last_activity_at: summary.last_activity_at.0,
        calls: summary.calls,
        errors: summary.errors,
        history_len: summary.history_len,
        waits: summary.waits.iter().map(wait_record).collect(),
        wakes_at: summary.wakes_at.map(|at| at.0),
        ending: summary.ending.as_ref().map(|ending| record::Ending {
            error: ending.error.clone(),
            message: ending.message.clone(),
            line: ending.line,
            signature: ending.signature.clone(),
            fingerprint: ending.fingerprint.clone(),
        }),
        quarantine: summary
            .quarantine
            .as_ref()
            .map(|note| record::QuarantineNote {
                kind: quarantine_kind_record(note.kind),
                reason: note.reason.clone(),
                since: note.since.0,
            }),
        actions: indexed.actions.clone(),
        call_errors: indexed
            .call_errors
            .iter()
            .map(|&kind| kind.into())
            .collect(),
    }
}

pub fn summary_of(record: record::Summary) -> Result<IndexedSummary, PartitionError> {
    Ok(IndexedSummary {
        summary: RunSummary {
            run: RunRef::new(tenant(record.tenant)?, record.run),
            namespace: namespace(record.namespace)?,
            origin: record.origin,
            suffix_len: record.suffix_len,
            code: record.code,
            headline: record.headline,
            action_set: record.action_set,
            status: record.status.into(),
            mode: record.mode.map(Into::into),
            labels: record.labels,
            source: record.source,
            workflow: record.workflow,
            started_by: record.started_by,
            started_at: record.started_at.map(Timestamp),
            ended_at: record.ended_at.map(Timestamp),
            last_activity_at: Timestamp(record.last_activity_at),
            calls: record.calls,
            errors: record.errors,
            history_len: record.history_len,
            waits: record.waits.into_iter().map(wait_of).collect(),
            wakes_at: record.wakes_at.map(Timestamp),
            ending: record.ending.map(|ending| Ending {
                error: ending.error,
                message: ending.message,
                line: ending.line,
                signature: ending.signature,
                fingerprint: ending.fingerprint,
            }),
            quarantine: record.quarantine.map(|note| QuarantineNote {
                kind: quarantine_kind_of(note.kind),
                reason: note.reason,
                since: Timestamp(note.since),
            }),
        },
        actions: record.actions,
        call_errors: record.call_errors.into_iter().map(Into::into).collect(),
    })
}

// -------------------------------------------------------------------------------------------
// Counters, gauges

pub fn runs_record(runs: &RunCounts) -> record::RunCounts {
    record::RunCounts {
        started: runs.started,
        ended: runs
            .ended
            .iter()
            .map(|(&status, &count)| (status.into(), count))
            .collect(),
    }
}

pub fn runs_of(record: record::RunCounts) -> RunCounts {
    RunCounts {
        started: record.started,
        ended: record
            .ended
            .into_iter()
            .map(|(status, count)| (status.into(), count))
            .collect(),
    }
}

fn sketch_record(sketch: &Sketch) -> record::Sketch {
    record::Sketch {
        count: sketch.count,
        sum_ms: sketch.sum_ms,
        max_ms: sketch.max_ms,
        buckets: sketch.buckets.clone(),
    }
}

fn sketch_of(record: record::Sketch) -> Sketch {
    Sketch {
        count: record.count,
        sum_ms: record.sum_ms,
        max_ms: record.max_ms,
        buckets: record.buckets,
    }
}

/// The counters of one namespace and hour; their `namespaces` are the store's to fill, never
/// stored.
pub fn counters_record(counters: &Counters) -> record::Counters {
    record::Counters {
        created: counters.created,
        runs: runs_record(&counters.runs),
        durations: sketch_record(&counters.durations),
        cpu_ms: u64::try_from(counters.cpu.as_millis()).unwrap_or(u64::MAX),
        workflows: counters
            .workflows
            .iter()
            .map(|(name, runs)| (name.clone(), runs_record(runs)))
            .collect(),
        failures: counters
            .failures
            .iter()
            .map(|(fingerprint, failures)| {
                let failures = record::FailureCounts {
                    signature: failures.signature.clone(),
                    error: failures.error.clone(),
                    runs: failures.runs,
                    scripts: failures.scripts.clone(),
                    lines: failures.lines.clone(),
                    latest: failures
                        .latest
                        .iter()
                        .map(|latest| record::LatestRun {
                            run: latest.run,
                            ended_at: latest.ended_at.0,
                            status: latest.status.into(),
                            message: latest.message.clone(),
                        })
                        .collect(),
                };
                (fingerprint.clone(), failures)
            })
            .collect(),
        actions: counters
            .actions
            .iter()
            .map(|(action, calls)| {
                let calls = record::ActionCounts {
                    calls: calls.calls,
                    completed: calls.completed,
                    failed: calls.failed,
                    errors: calls
                        .errors
                        .iter()
                        .map(|(&kind, &count)| (kind.into(), count))
                        .collect(),
                    redeliveries: calls.redeliveries,
                    latency: sketch_record(&calls.latency),
                };
                (action.clone(), calls)
            })
            .collect(),
        scripts: counters
            .scripts
            .iter()
            .map(|(&code, runs)| {
                let runs = record::ScriptCounts {
                    runs: runs_record(&runs.runs),
                    actions: runs.actions.clone(),
                    first_started_at: runs.first_started_at.map(|at| at.0),
                    last_started_at: runs.last_started_at.map(|at| at.0),
                };
                (code, runs)
            })
            .collect(),
    }
}

pub fn counters_of(record: record::Counters) -> Counters {
    Counters {
        created: record.created,
        runs: runs_of(record.runs),
        durations: sketch_of(record.durations),
        cpu: Duration::from_millis(record.cpu_ms),
        workflows: record
            .workflows
            .into_iter()
            .map(|(name, runs)| (name, runs_of(runs)))
            .collect(),
        namespaces: BTreeMap::new(),
        failures: record
            .failures
            .into_iter()
            .map(|(fingerprint, failures)| {
                let failures = FailureCounts {
                    signature: failures.signature,
                    error: failures.error,
                    runs: failures.runs,
                    scripts: failures.scripts,
                    lines: failures.lines,
                    latest: failures
                        .latest
                        .into_iter()
                        .map(|latest| LatestRun {
                            run: latest.run,
                            ended_at: Timestamp(latest.ended_at),
                            status: latest.status.into(),
                            message: latest.message,
                        })
                        .collect(),
                };
                (fingerprint, failures)
            })
            .collect(),
        actions: record
            .actions
            .into_iter()
            .map(|(action, calls)| {
                let calls = ActionCounts {
                    calls: calls.calls,
                    completed: calls.completed,
                    failed: calls.failed,
                    errors: calls
                        .errors
                        .into_iter()
                        .map(|(kind, count)| (kind.into(), count))
                        .collect(),
                    redeliveries: calls.redeliveries,
                    latency: sketch_of(calls.latency),
                };
                (action, calls)
            })
            .collect(),
        scripts: record
            .scripts
            .into_iter()
            .map(|(code, runs)| {
                let runs = ScriptCounts {
                    runs: runs_of(runs.runs),
                    actions: runs.actions,
                    first_started_at: runs.first_started_at.map(Timestamp),
                    last_started_at: runs.last_started_at.map(Timestamp),
                };
                (code, runs)
            })
            .collect(),
    }
}

pub fn waiting_record(kind: WaitingFor) -> record::WaitingFor {
    match kind {
        WaitingFor::Signal => record::WaitingFor::Signal,
        WaitingFor::Timer => record::WaitingFor::Timer,
        WaitingFor::Call => record::WaitingFor::Call,
        WaitingFor::Harness => record::WaitingFor::Harness,
        WaitingFor::Approval => record::WaitingFor::Approval,
        WaitingFor::Parked => record::WaitingFor::Parked,
        WaitingFor::Deferred => record::WaitingFor::Deferred,
    }
}

pub fn waiting_of(kind: record::WaitingFor) -> WaitingFor {
    match kind {
        record::WaitingFor::Signal => WaitingFor::Signal,
        record::WaitingFor::Timer => WaitingFor::Timer,
        record::WaitingFor::Call => WaitingFor::Call,
        record::WaitingFor::Harness => WaitingFor::Harness,
        record::WaitingFor::Approval => WaitingFor::Approval,
        record::WaitingFor::Parked => WaitingFor::Parked,
        record::WaitingFor::Deferred => WaitingFor::Deferred,
    }
}

pub fn gauges_record(gauges: &Gauges) -> record::Gauges {
    record::Gauges {
        active_runs: gauges.active_runs,
        waiting: gauges
            .waiting
            .iter()
            .map(|(&kind, &count)| (waiting_record(kind), count))
            .collect(),
        quarantined: gauges.quarantined,
        scripts: gauges.scripts.clone(),
    }
}

pub fn gauges_of(record: record::Gauges) -> Gauges {
    Gauges {
        active_runs: record.active_runs,
        waiting: record
            .waiting
            .into_iter()
            .map(|(kind, count)| (waiting_of(kind), count))
            .collect(),
        quarantined: record.quarantined,
        scripts: record.scripts,
    }
}

// -------------------------------------------------------------------------------------------
// The outbox and the organizations

fn attribute_record(value: &AttributeValue) -> record::Attribute {
    match value {
        AttributeValue::String(text) => record::Attribute::String(text.clone()),
        AttributeValue::Int(value) => record::Attribute::Int(*value),
        AttributeValue::Float(value) => record::Attribute::Float(*value),
        AttributeValue::Bool(value) => record::Attribute::Bool(*value),
    }
}

fn attribute_of(record: record::Attribute) -> AttributeValue {
    match record {
        record::Attribute::String(text) => AttributeValue::String(text),
        record::Attribute::Int(value) => AttributeValue::Int(value),
        record::Attribute::Float(value) => AttributeValue::Float(value),
        record::Attribute::Bool(value) => AttributeValue::Bool(value),
    }
}

fn attributes_record(
    attributes: &BTreeMap<String, AttributeValue>,
) -> BTreeMap<String, record::Attribute> {
    attributes
        .iter()
        .map(|(key, value)| (key.clone(), attribute_record(value)))
        .collect()
}

fn attributes_of(
    attributes: BTreeMap<String, record::Attribute>,
) -> BTreeMap<String, AttributeValue> {
    attributes
        .into_iter()
        .map(|(key, value)| (key, attribute_of(value)))
        .collect()
}

fn span_record(span: &Span) -> record::Span {
    record::Span {
        trace_id: span.trace_id.0,
        span_id: span.span_id.0,
        parent: span.parent.map(|parent| parent.0),
        name: span.name.clone(),
        kind: match span.kind {
            SpanKind::Internal => record::SpanKind::Internal,
            SpanKind::Client => record::SpanKind::Client,
        },
        start: span.start.0,
        end: span.end.0,
        status: match &span.status {
            SpanStatus::Unset => record::SpanStatus::Unset {},
            SpanStatus::Ok => record::SpanStatus::Ok {},
            SpanStatus::Error { message } => record::SpanStatus::Error {
                message: message.clone(),
            },
        },
        attributes: attributes_record(&span.attributes),
    }
}

fn span_of(record: record::Span) -> Span {
    Span {
        trace_id: TraceId(record.trace_id),
        span_id: SpanId(record.span_id),
        parent: record.parent.map(SpanId),
        name: record.name,
        kind: match record.kind {
            record::SpanKind::Internal => SpanKind::Internal,
            record::SpanKind::Client => SpanKind::Client,
        },
        start: Timestamp(record.start),
        end: Timestamp(record.end),
        status: match record.status {
            record::SpanStatus::Unset {} => SpanStatus::Unset,
            record::SpanStatus::Ok {} => SpanStatus::Ok,
            record::SpanStatus::Error { message } => SpanStatus::Error { message },
        },
        attributes: attributes_of(record.attributes),
    }
}

fn log_record(log: &LogRecord) -> record::Log {
    record::Log {
        trace_id: log.trace_id.0,
        span_id: log.span_id.map(|span| span.0),
        time: log.time.0,
        level: log.level.into(),
        message: log.message.clone(),
        attributes: attributes_record(&log.attributes),
    }
}

fn log_of(record: record::Log) -> LogRecord {
    LogRecord {
        trace_id: TraceId(record.trace_id),
        span_id: record.span_id.map(SpanId),
        time: Timestamp(record.time),
        level: record.level.into(),
        message: record.message,
        attributes: attributes_of(record.attributes),
    }
}

pub fn alert_record(alert: &QuotaAlert) -> record::QuotaAlert {
    let key = &alert.key;
    record::QuotaAlert {
        tenant: key.tenant.to_string(),
        namespace: key.namespace.as_ref().map(ToString::to_string),
        quota: match key.quota {
            QuotaKind::CpuPerMonth => record::QuotaKind::CpuPerMonth,
            QuotaKind::ActiveRuns => record::QuotaKind::ActiveRuns,
            QuotaKind::RunsPerMonth => record::QuotaKind::RunsPerMonth,
            QuotaKind::Workflows => record::QuotaKind::Workflows,
            QuotaKind::MaxTtl => record::QuotaKind::MaxTtl,
            QuotaKind::MaxCodeLines => record::QuotaKind::MaxCodeLines,
        },
        year: key.period.year(),
        month: key.period.month(),
        threshold: match key.threshold {
            Threshold::Near => record::Threshold::Near,
            Threshold::Reached => record::Threshold::Reached,
        },
        used: alert.used,
        limit: alert.limit,
        raised_at: alert.raised_at.0,
        sent_at: alert.sent_at.map(|at| at.0),
    }
}

pub fn alert_of(record: record::QuotaAlert) -> Result<QuotaAlert, PartitionError> {
    Ok(QuotaAlert {
        key: QuotaAlertKey {
            tenant: tenant(record.tenant)?,
            namespace: record.namespace.map(namespace).transpose()?,
            quota: match record.quota {
                record::QuotaKind::CpuPerMonth => QuotaKind::CpuPerMonth,
                record::QuotaKind::ActiveRuns => QuotaKind::ActiveRuns,
                record::QuotaKind::RunsPerMonth => QuotaKind::RunsPerMonth,
                record::QuotaKind::Workflows => QuotaKind::Workflows,
                record::QuotaKind::MaxTtl => QuotaKind::MaxTtl,
                record::QuotaKind::MaxCodeLines => QuotaKind::MaxCodeLines,
            },
            period: Month::new(record.year, record.month).ok_or_else(|| {
                corrupt(format!(
                    "the month {}-{} of a quota alert",
                    record.year, record.month
                ))
            })?,
            threshold: match record.threshold {
                record::Threshold::Near => Threshold::Near,
                record::Threshold::Reached => Threshold::Reached,
            },
        },
        used: record.used,
        limit: record.limit,
        raised_at: Timestamp(record.raised_at),
        sent_at: record.sent_at.map(Timestamp),
    })
}

fn webhook_event_record(event: &WebhookEvent) -> record::WebhookEvent {
    record::WebhookEvent {
        id: event.id.clone(),
        tenant: event.tenant.to_string(),
        namespace: event.namespace.as_ref().map(ToString::to_string),
        run: event.run,
        labels: event.labels.clone(),
        occurred_at: event.occurred_at.0,
        kind: match &event.kind {
            WebhookEventKind::RunCompleted { output } => record::WebhookKind::RunCompleted {
                output: output.clone(),
            },
            WebhookEventKind::RunFailed { status, error } => record::WebhookKind::RunFailed {
                status: (*status).into(),
                error: error.clone(),
            },
            WebhookEventKind::RunSuspended { waiting_for } => record::WebhookKind::RunSuspended {
                waiting_for: waiting_for.clone(),
            },
            WebhookEventKind::RunQuarantined { reason } => record::WebhookKind::RunQuarantined {
                reason: reason.clone(),
            },
            WebhookEventKind::ActionSchemaChanged {
                seq,
                action,
                message,
            } => record::WebhookKind::ActionSchemaChanged {
                seq: seq.0,
                action: action.clone(),
                message: message.clone(),
            },
            WebhookEventKind::CredentialsExpired {
                seq,
                action,
                message,
                reason,
                secret,
            } => record::WebhookKind::CredentialsExpired {
                seq: seq.0,
                action: action.clone(),
                message: message.clone(),
                reason: match reason {
                    CredentialsExpiredReason::Refused => record::CredentialsReason::Refused,
                    CredentialsExpiredReason::Missing => record::CredentialsReason::Missing,
                },
                secret: secret.clone(),
            },
            WebhookEventKind::QuotaAlert(alert) => record::WebhookKind::QuotaAlert {
                alert: alert_record(alert),
            },
        },
    }
}

fn webhook_event_of(record: record::WebhookEvent) -> Result<WebhookEvent, PartitionError> {
    Ok(WebhookEvent {
        id: record.id,
        tenant: tenant(record.tenant)?,
        namespace: record.namespace.map(namespace).transpose()?,
        run: record.run,
        labels: record.labels,
        occurred_at: Timestamp(record.occurred_at),
        kind: match record.kind {
            record::WebhookKind::RunCompleted { output } => {
                WebhookEventKind::RunCompleted { output }
            }
            record::WebhookKind::RunFailed { status, error } => WebhookEventKind::RunFailed {
                status: status.into(),
                error,
            },
            record::WebhookKind::RunSuspended { waiting_for } => {
                WebhookEventKind::RunSuspended { waiting_for }
            }
            record::WebhookKind::RunQuarantined { reason } => {
                WebhookEventKind::RunQuarantined { reason }
            }
            record::WebhookKind::ActionSchemaChanged {
                seq,
                action,
                message,
            } => WebhookEventKind::ActionSchemaChanged {
                seq: Seq(seq),
                action,
                message,
            },
            record::WebhookKind::CredentialsExpired {
                seq,
                action,
                message,
                reason,
                secret,
            } => WebhookEventKind::CredentialsExpired {
                seq: Seq(seq),
                action,
                message,
                reason: match reason {
                    record::CredentialsReason::Refused => CredentialsExpiredReason::Refused,
                    record::CredentialsReason::Missing => CredentialsExpiredReason::Missing,
                },
                secret,
            },
            record::WebhookKind::QuotaAlert { alert } => {
                WebhookEventKind::QuotaAlert(alert_of(alert)?)
            }
        },
    })
}

pub fn outbox_record(entry: &OutboxEntry) -> record::Outbox {
    record::Outbox {
        id: entry.id.0,
        tenant: entry.tenant.to_string(),
        run: entry.run,
        projection: projection_record(&entry.projection),
    }
}

/// What an outbox entry delivers.
pub fn projection_record(projection: &Projection) -> record::Projection {
    match projection {
        Projection::Otel { spans, logs } => record::Projection::Otel {
            spans: spans.iter().map(span_record).collect(),
            logs: logs.iter().map(log_record).collect(),
        },
        Projection::Webhooks(event) => record::Projection::Webhooks {
            event: webhook_event_record(event),
        },
        Projection::QuotaAlert(alert) => record::Projection::QuotaAlert {
            alert: alert_record(alert),
        },
    }
}

pub fn outbox_of(record: record::Outbox) -> Result<OutboxEntry, PartitionError> {
    Ok(OutboxEntry {
        id: OutboxId(record.id),
        tenant: tenant(record.tenant)?,
        run: record.run,
        projection: projection_of(record.projection)?,
    })
}

pub fn projection_of(record: record::Projection) -> Result<Projection, PartitionError> {
    Ok(match record {
        record::Projection::Otel { spans, logs } => Projection::Otel {
            spans: spans.into_iter().map(span_of).collect(),
            logs: logs.into_iter().map(log_of).collect(),
        },
        record::Projection::Webhooks { event } => Projection::Webhooks(webhook_event_of(event)?),
        record::Projection::QuotaAlert { alert } => Projection::QuotaAlert(alert_of(alert)?),
    })
}

pub fn organization_record(state: &OrganizationState) -> record::Organization {
    record::Organization {
        alerts: state.alerts.iter().map(alert_record).collect(),
        admissions: state
            .admissions
            .iter()
            .map(|admission| record::Admission {
                run: admission.run,
                namespace: admission.namespace.to_string(),
                code: admission.code,
                admitted_at: admission.admitted_at.0,
            })
            .collect(),
    }
}

pub fn organization_of(record: record::Organization) -> Result<OrganizationState, PartitionError> {
    Ok(OrganizationState {
        alerts: record
            .alerts
            .into_iter()
            .map(alert_of)
            .collect::<Result<_, _>>()?,
        admissions: record
            .admissions
            .into_iter()
            .map(|admission| {
                Ok(Admission {
                    run: admission.run,
                    namespace: namespace(admission.namespace)?,
                    code: admission.code,
                    admitted_at: Timestamp(admission.admitted_at),
                })
            })
            .collect::<Result<_, PartitionError>>()?,
    })
}
