//! Host tools: functions of the harness, executed by its SDK worker, which long-polls
//! `GET /v1/tasks` and answers through the claim's token (`archi/04-tools.md`, "Routes").
//! Long-polling is the server's job; these are the use cases behind each call.
//!
//! ```text
//! GET  /v1/tasks                  claim_host_tasks   (a claim per task, with its token)
//! POST /v1/tasks/{token}/heartbeat heartbeat_task    (the claim lasts one more heartbeat timeout)
//! POST /v1/tasks/{token}/defer     defer_task        (answered later, until a deadline)
//! POST /v1/tasks/{token}/complete  complete_task     (the result, to the script)
//! POST /v1/tasks/{token}/fail      fail_task         (retried with backoff, or a ToolError)
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use ptc_core::{
    Digest, Interruption, JsonValue, Timestamp, ToolFailure, ToolFailureKind, ToolOutcome,
};

use crate::engine::{Engine, EngineError};
use crate::ports::{
    BlobStore, ClaimedTask, Clock, Entropy, JournalStore, ScriptExecutor, SecretStore, TaskFilter,
    TaskRef, TaskState, TaskToken, TaskUpdate, TenantId, ToolDispatcher, ToolTask, TraceContext,
    WebhookSender,
};

/// What a harness's worker claims (`GET /v1/tasks`): the host tools of one tenant, optionally
/// of one toolset and some tools only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostClaim {
    pub tenant: TenantId,
    pub toolset: Option<Digest>,
    /// Empty: every host tool.
    pub tools: BTreeSet<String>,
    pub limit: usize,
}

/// A host-tool call handed to a harness's worker. The worker keeps the claim alive with
/// heartbeats and answers through the token.
#[derive(Debug, Clone, PartialEq)]
pub struct HostTask {
    /// The claim's secret token. It changes with each claim.
    pub token: TaskToken,
    pub task: TaskRef,
    pub tool: String,
    pub args: JsonValue,
    /// The same on every attempt: the handler recognizes a call it already made.
    pub idempotency_key: String,
    /// From 1. Above 1, an earlier attempt failed or was interrupted.
    pub attempt: u32,
    /// The claim expires if no heartbeat comes within this time.
    pub heartbeat_timeout: Duration,
    /// The tool's execution timeout: past it, the call is not waited for any more. `None` for a
    /// host tool without one, the default: the run's TTL bounds it.
    pub execution_timeout: Option<Duration>,
    /// The run's labels.
    pub labels: BTreeMap<String, String>,
    pub trace_context: Option<TraceContext>,
}

/// A failure reported by a host tool's handler (`POST /v1/tasks/{token}/fail`).
#[derive(Debug, Clone, PartialEq)]
pub struct HostFailure {
    /// The message of the `ToolError`, written for the model.
    pub message: String,
    /// `ToolError.data`.
    pub data: Option<JsonValue>,
    /// A transient failure: under `retry`, the call is made again after a backoff, up to the
    /// maximum of attempts. Delivered, it has the kind `unavailable`; otherwise `error`.
    pub retryable: bool,
}

impl<J, B, X, D, C, E, S, W> Engine<J, B, X, D, C, E, S, W>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
    S: SecretStore,
    W: WebhookSender,
{
    /// Claims host-tool tasks for a harness's worker. A task whose previous claim expired is
    /// handed out again under `retry`, up to the maximum of attempts; under `at_most_once`, or
    /// past it, it fails with `outcome_unknown` instead and is not handed out.
    pub async fn claim_host_tasks(&self, claim: &HostClaim) -> Result<Vec<HostTask>, EngineError> {
        let filter = TaskFilter::Host {
            tenant: claim.tenant.clone(),
            toolset: claim.toolset,
            tools: claim.tools.clone(),
        };
        let claimed = self
            .journal
            .claim_tool_tasks(&self.config.worker, &filter, self.clock.now(), claim.limit)
            .await?;
        let mut tasks = Vec::with_capacity(claimed.len());
        for ClaimedTask {
            task,
            token,
            previous_claim_expired,
        } in claimed
        {
            if let Some(failure) = self.interrupted(&task, previous_claim_expired) {
                self.finish(&task, &token, ToolOutcome::Failure(failure))
                    .await?;
                continue;
            }
            match self.host_task(&task, &token).await {
                Ok(Some(host_task)) => tasks.push(host_task),
                Ok(None) => {}
                // Not handed out: it is safe to hand it out again later, whatever its policy.
                Err(error) => {
                    tracing::error!(task = %task.task, %error, "a host task could not be prepared");
                    self.retry_later(&task, &token).await?;
                }
            }
        }
        Ok(tasks)
    }

    /// Extends the claim of `token` by its heartbeat timeout. Returns when the claim now ends
    /// (for a deferred task, its unchanged deadline). [`EngineError::TaskGone`] tells the
    /// worker to stop: the task was answered, its run ended, or another worker took it over.
    pub async fn heartbeat_task(
        &self,
        tenant: &TenantId,
        token: &TaskToken,
    ) -> Result<Timestamp, EngineError> {
        self.task_of(tenant, token).await?;
        self.journal
            .heartbeat_tool_task(token, self.clock.now())
            .await?
            .ok_or(EngineError::TaskGone)
    }

    /// The handler answers later, through the same token: the task is deferred, without
    /// heartbeats, until `timeout` from now, bounded by the tool's execution timeout and by the
    /// run's deadline (the default). Late, it fails with `timeout`. Returns the deadline.
    pub async fn defer_task(
        &self,
        tenant: &TenantId,
        token: &TaskToken,
        timeout: Option<Duration>,
    ) -> Result<Timestamp, EngineError> {
        let task = self.task_of(tenant, token).await?;
        if let TaskState::Deferred { until } = task.state {
            return Ok(until);
        }
        let record = self.run_record(&task.task.run).await?;
        let until = self.deferral_deadline(&task, &record, timeout);
        match self.journal.defer_tool_task(token, until).await? {
            TaskUpdate::Applied => Ok(until),
            TaskUpdate::InvalidState(TaskState::Deferred { until }) => Ok(until),
            TaskUpdate::NotFound | TaskUpdate::InvalidState(_) => Err(EngineError::TaskGone),
        }
    }

    /// The handler's result, delivered to the script. A result above the payload limit is
    /// refused ([`EngineError::ResultTooLarge`]): the worker may fail the task instead.
    pub async fn complete_task(
        &self,
        tenant: &TenantId,
        token: &TaskToken,
        result: JsonValue,
    ) -> Result<(), EngineError> {
        let result = self.bounded_result(result)?;
        let task = self.task_of(tenant, token).await?;
        match self
            .finish(&task, token, ToolOutcome::Result(result))
            .await?
        {
            TaskUpdate::Applied => Ok(()),
            _ => Err(EngineError::TaskGone),
        }
    }

    /// The handler failed. A retryable failure of a claimed `retry` task with attempts left
    /// puts it back to `ready` after a backoff; any other reaches the script as a `ToolError`.
    pub async fn fail_task(
        &self,
        tenant: &TenantId,
        token: &TaskToken,
        failure: HostFailure,
    ) -> Result<(), EngineError> {
        let task = self.task_of(tenant, token).await?;
        let retry = failure.retryable
            && matches!(task.state, TaskState::Claimed { .. })
            && task.policy.on_interruption == Interruption::Retry
            && task.attempt < self.config.max_tool_attempts;
        let update = if retry {
            self.retry_later(&task, token).await?
        } else {
            let failure = ToolFailure {
                kind: if failure.retryable {
                    ToolFailureKind::Unavailable
                } else {
                    ToolFailureKind::Error
                },
                message: failure.message,
                data: failure.data,
            };
            self.finish(&task, token, ToolOutcome::Failure(failure))
                .await?
        };
        match update {
            TaskUpdate::Applied => Ok(()),
            _ => Err(EngineError::TaskGone),
        }
    }

    /// The task holding `token`, if it belongs to `tenant`: a token of another tenant is as
    /// good as unknown, and so is a token with a NUL character, which no store holds (Postgres
    /// refuses it in a query).
    pub(super) async fn task_of(
        &self,
        tenant: &TenantId,
        token: &TaskToken,
    ) -> Result<ToolTask, EngineError> {
        if token.0.contains('\0') {
            return Err(EngineError::TaskGone);
        }
        self.journal
            .tool_task(token)
            .await?
            .filter(|task| task.task.run.tenant == *tenant)
            .ok_or(EngineError::TaskGone)
    }

    /// The call as the worker receives it. `None` when the run has ended.
    async fn host_task(
        &self,
        task: &ToolTask,
        token: &TaskToken,
    ) -> Result<Option<HostTask>, EngineError> {
        let Some(record) = self.journal.get_run(&task.task.run).await? else {
            return Ok(None);
        };
        if record.status.is_terminal() {
            return Ok(None);
        }
        Ok(Some(HostTask {
            token: token.clone(),
            task: task.task.clone(),
            tool: task.tool.clone(),
            args: self.task_args(task).await?,
            idempotency_key: self.task_key(&record, task).await?,
            attempt: task.attempt,
            heartbeat_timeout: task.policy.heartbeat_timeout,
            execution_timeout: task.policy.execution_timeout,
            trace_context: self.call_trace_context(&record, task.task.seq),
            labels: record.metadata.labels,
        }))
    }
}
