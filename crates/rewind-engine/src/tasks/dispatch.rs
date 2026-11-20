//! The tasks Rewind calls itself: MCP tools, API tools and built-ins, and every tool in mock mode
//! (`archi/04-tools.md`, "Tool task lifecycle").
//!
//! ```text
//! claim ──► interrupted before? ── at_most_once, or too many attempts ──► done: outcome_unknown
//!   │
//!   ▼ prepare: the pinned tool, its args, its server's or API's auth resolved: the secret the
//!   │           run attached, or the tenant's signing key (a failure here puts the task back
//!   │           with backoff: the call was not sent; a secret that no longer exists parks it,
//!   │           secret_missing)
//!   ▼ dispatch, the claim heartbeated meanwhile (or, in the engine, the `webhook` built-in and
//!   │  the workflows' built-ins)
//!   │
//!   ├─ Result, Failed ──────────────────────────────────────────────► done
//!   ├─ Transient ── retry and attempts left ── ready after backoff; else done with the failure
//!   ├─ Deferred ─── until the execution timeout, bounded by the run's deadline
//!   └─ Park ─────── parked, then tool.schema_changed / credentials.expired (refused, missing)
//!                   to subscriptions
//! ```

use std::time::Duration;

use tracing::Instrument as _;

use rewind_core::{
    Builtin, Event, Interruption, JsonValue, Lifecycle, Payload, RunMode, SecretEncoding,
    SecretPlacement, SecretRef, SecretScope, Seq, Timestamp, ToolAuth, ToolFailure,
    ToolFailureKind, ToolOutcome, ToolRoute, WorkflowOp,
};
use serde_json::json;

use super::InFlight;
use crate::driver::ToolOrder;
use crate::engine::{Engine, EngineError, idempotency_key};
use crate::ports::{
    BlobStore, CallAuth, CallClaims, ClaimedTask, Clock, CredentialsExpiredReason, Dispatched,
    Entropy, JournalStore, ParkKind, ParkReason, RETRY_AFTER_DATA, RunRecord, RunRef,
    ScriptExecutor, SecretId, SecretReader, SecretStore, TaskPhase, TaskProgress, TaskToken,
    TaskUpdate, ToolCall, ToolDispatcher, ToolTask, TraceContext, WebhookEvent, WebhookEventKind,
    WebhookSender,
};
use crate::{metrics, payloads, text};

/// A call ready to be dispatched.
struct Prepared {
    record: RunRecord,
    call: ToolCall,
    /// How the call proves who calls.
    auth: Auth,
    /// The engine answers the call itself, or turns the dispatcher's answer into its own.
    served: Option<Served>,
}

/// How a prepared call proves who calls, or why it is not sent.
enum Auth {
    /// Sent with this.
    Ready(CallAuth),
    /// The secret the run attached for the call's server no longer exists: the call is parked
    /// (`secret_missing`) instead of sent, until the secret is put again.
    SecretMissing { server: String, secret: SecretRef },
    /// The call cannot prove who calls, for good: it fails without being sent.
    Refused(ToolFailure),
}

/// A call the engine serves: it needs the store, which no dispatcher holds.
enum Served {
    /// The `webhook` built-in of a live run.
    Webhook,
    /// A workflow's built-in in a live run: the engine starts or signals the run.
    Workflow { workflow: String, op: WorkflowOp },
    /// A workflow's start in mock mode: the mocks answer with the child's end, which the engine
    /// turns into a start and an end.
    MockStart,
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
    /// Claims up to `limit` tool tasks for this worker, among those its
    /// [`tool_tasks`](crate::EngineConfig::tool_tasks) filter selects.
    pub async fn claim_tool_tasks(&self, limit: usize) -> Result<Vec<ClaimedTask>, EngineError> {
        Ok(self
            .journal
            .claim_tool_tasks(
                &self.config.worker,
                &self.config.tool_tasks,
                self.clock.now(),
                limit,
            )
            .await?)
    }

    /// Executes a claimed tool task, and reports what became of it to the store.
    ///
    /// A task claimed again after its previous claim expired (its dispatcher died mid-call) is
    /// called again with the same idempotency key, so that the tool can tell it apart from a
    /// new call (`archi/04-tools.md`); under `at_most_once`, or past
    /// [`max_tool_attempts`](crate::EngineConfig::max_tool_attempts), it fails with
    /// `outcome_unknown` instead. While the dispatcher works, [`heartbeat_tool_claims`] keeps
    /// the claim alive.
    ///
    /// An error means the call could not be prepared (a missing toolset or payload, a store or
    /// secret store failure): it was not sent, and the task is ready again after a backoff.
    ///
    /// Returns what the task waits for after this attempt when it waits for the harness: parked
    /// or deferred, with its attempt, for the caller to record it in the read model. `None` when
    /// it was answered, is put back for another attempt (its next claim records it), or is gone.
    ///
    /// [`heartbeat_tool_claims`]: Self::heartbeat_tool_claims
    pub async fn run_tool_task(
        &self,
        claimed: ClaimedTask,
    ) -> Result<Option<TaskProgress>, EngineError> {
        let ClaimedTask {
            task,
            token,
            previous_claim_expired,
        } = claimed;
        if let Some(failure) = self.interrupted(&task, previous_claim_expired) {
            self.finish(&task, &token, ToolOutcome::Failure(failure))
                .await?;
            return Ok(None);
        }
        let prepared = match self.prepare(&task, &token).await {
            Ok(Some(prepared)) => prepared,
            // The run has ended: its tasks are gone.
            Ok(None) => return Ok(None),
            Err(error) => {
                tracing::error!(task = %task.task, %error, "a tool call could not be prepared");
                self.retry_later(&task, &token).await?;
                return Err(error);
            }
        };

        let now = self.clock.now();
        let Prepared {
            record,
            call,
            auth,
            served,
        } = prepared;
        let auth = match auth {
            Auth::Ready(auth) => auth,
            Auth::SecretMissing { server, secret } => {
                let put = match secret.scope {
                    SecretScope::Organization => format!("PUT /v1/secrets/{}", secret.name),
                    SecretScope::Namespace => format!(
                        "PUT /v1/namespaces/{}/secrets/{}",
                        record.namespace, secret.name
                    ),
                };
                let reason = ParkReason {
                    kind: ParkKind::SecretMissing,
                    message: format!(
                        "the secret {secret} the run attached for {server} no longer exists: \
                         put it again ({put}), and the call is sent with the same idempotency key"
                    ),
                };
                return self
                    .settle(&task, &token, &record, Dispatched::Park(reason))
                    .await;
            }
            Auth::Refused(failure) => {
                return self
                    .settle(&task, &token, &record, Dispatched::Failed(failure))
                    .await;
            }
        };
        // The attempt span: a child of the tool call span, through the call's trace context.
        let attempt = tracing::info_span!(
            "tool_attempt",
            task = %task.task,
            attempt = task.attempt,
            traceparent = call.trace_context.as_ref().map(|context| context.traceparent.as_str()),
            histogram.rewind.tool_attempt.duration = tracing::field::Empty,
            rewind.tool = task.tool.as_str(),
            rewind.outcome = tracing::field::Empty,
        );
        // In mock mode, the end of the child a start stands for follows the start's result.
        let mut mock_end = None;
        let caller = call.run.clone();
        let dispatched = async {
            match &served {
                Some(Served::Webhook) => return self.webhook_builtin(&call).await,
                Some(Served::Workflow {
                    workflow,
                    op: WorkflowOp::Start,
                }) => return self.start_child(&call, &record, workflow).await,
                Some(Served::Workflow {
                    workflow,
                    op: WorkflowOp::Signal,
                }) => return self.signal_run(&call, &record, workflow).await,
                Some(Served::MockStart) | None => {}
            }
            let in_flight = InFlight {
                heartbeat_timeout: task.policy.heartbeat_timeout,
                renewed_at: now,
                deadline: task
                    .policy
                    .execution_timeout
                    .map(|timeout| now.saturating_add(timeout)),
            };
            let _dispatching = self.tool_tasks.dispatching(&token, in_flight);
            if matches!(served, Some(Served::MockStart)) {
                let answered = self.dispatcher.dispatch(call.clone(), auth).await;
                let (dispatched, end) = self.mock_start(answered);
                mock_end = end;
                return dispatched;
            }
            let texts = secret_texts(&auth);
            redacted(self.dispatcher.dispatch(call, auth).await, &texts)
        }
        .instrument(attempt.clone())
        .await;
        attempt.record(metrics::OUTCOME, metrics::attempt_outcome(&dispatched));
        drop(attempt);
        let progress = self.settle(&task, &token, &record, dispatched).await?;
        if let Some((child, end)) = mock_end {
            self.enqueue_mock_end(&caller, child, end).await?;
        }
        Ok(progress)
    }

    /// Claims up to `limit` tool tasks and runs them in order. Returns how many ran.
    pub async fn dispatch_tool_calls(&self, limit: usize) -> Result<usize, EngineError> {
        let tasks = self.claim_tool_tasks(limit).await?;
        let count = tasks.len();
        for task in tasks {
            self.run_tool_task(task).await?;
        }
        Ok(count)
    }

    /// Renews the claims of the calls this engine is dispatching, once a third of their
    /// heartbeat timeout has passed since their last renewal. A call past its execution timeout
    /// is no longer renewed: its claim expires, and the call is attempted again, or fails with
    /// `outcome_unknown` under `at_most_once`. A worker that dispatches calls concurrently calls
    /// this every second or so. Returns how many claims were renewed.
    pub async fn heartbeat_tool_claims(&self) -> Result<usize, EngineError> {
        let now = self.clock.now();
        let mut renewed = 0;
        for token in self.tool_tasks.due_heartbeats(now) {
            if self
                .journal
                .heartbeat_tool_task(&token, now)
                .await?
                .is_some()
            {
                self.tool_tasks.renewed(&token, now);
                renewed += 1;
            } else {
                // Done or taken over: what the dispatcher reports for it will be dropped.
                self.tool_tasks.dispatched(&token);
            }
        }
        Ok(renewed)
    }

    /// Ends the deferred tasks whose deadline passed, with a `timeout` failure. Returns how
    /// many.
    pub async fn expire_deferred_tool_tasks(&self) -> Result<usize, EngineError> {
        Ok(self
            .journal
            .expire_deferred_tool_tasks(self.clock.now(), self.config.scheduler_batch)
            .await?)
    }

    /// The earliest time a call retried after a backoff becomes due, if it is later than now:
    /// in virtual time, the driver jumps there, as it does to a timer. The store holds it, so
    /// an engine restarted during a backoff still wakes up for it.
    pub async fn next_tool_wake_up(&self) -> Result<Option<Timestamp>, EngineError> {
        Ok(self.journal.next_tool_attempt(self.clock.now()).await?)
    }

    /// One pass of the driver over tool tasks: ends the late deferred tasks, then claims up to
    /// `batch` tasks and runs them in `order`. Returns how many tasks moved.
    pub(crate) async fn tool_pass(
        &self,
        order: ToolOrder,
        batch: usize,
    ) -> Result<usize, EngineError> {
        let expired = self.expire_deferred_tool_tasks().await?;
        let mut tasks = self.claim_tool_tasks(batch).await?;
        if order == ToolOrder::Reversed {
            tasks.reverse();
        }
        let ran = tasks.len();
        for task in tasks {
            self.run_tool_task(task).await?;
        }
        Ok(expired + ran)
    }

    // ---------------------------------------------------------------------------------------
    // Helpers shared with the host-tool and harness use cases

    /// The failure of a task claimed again after its claim expired, when it must not be called
    /// again: always under `at_most_once`, past the maximum of attempts under `retry`.
    pub(super) fn interrupted(
        &self,
        task: &ToolTask,
        previous_claim_expired: bool,
    ) -> Option<ToolFailure> {
        if !previous_claim_expired {
            return None;
        }
        let message = match task.policy.on_interruption {
            Interruption::AtMostOnce => format!(
                "the call to {} was interrupted and may or may not have taken effect; the tool \
                 is at_most_once, so it is not called again",
                task.tool
            ),
            Interruption::Retry if task.attempt > self.config.max_tool_attempts => format!(
                "the call to {} was interrupted after {} attempts and may or may not have taken \
                 effect",
                task.tool,
                task.attempt - 1
            ),
            Interruption::Retry => return None,
        };
        Some(ToolFailure {
            kind: ToolFailureKind::OutcomeUnknown,
            message,
            data: None,
        })
    }

    /// Ends the task with `outcome`, delivered to its run. An outcome for a task that no longer
    /// exists (answered, its run ended, its claim taken over) is dropped.
    pub(super) async fn finish(
        &self,
        task: &ToolTask,
        token: &TaskToken,
        outcome: ToolOutcome,
    ) -> Result<TaskUpdate, EngineError> {
        let update = self
            .journal
            .complete_tool_task(token, outcome, self.clock.now())
            .await?;
        if update != TaskUpdate::Applied {
            tracing::debug!(task = %task.task, ?update, "tool outcome dropped");
        }
        Ok(update)
    }

    /// Puts a claimed task back to `ready` after the backoff of its attempt. The jitter is
    /// drawn from the call's `{run_id}/{seq}`.
    pub(super) async fn retry_later(
        &self,
        task: &ToolTask,
        token: &TaskToken,
    ) -> Result<TaskUpdate, EngineError> {
        self.retry_after(task, token, Duration::ZERO).await
    }

    /// As [`retry_later`](Self::retry_later), never sooner than `at_least` from now: the wait a
    /// server asked for (`Retry-After`).
    pub(super) async fn retry_after(
        &self,
        task: &ToolTask,
        token: &TaskToken,
        at_least: Duration,
    ) -> Result<TaskUpdate, EngineError> {
        let call = idempotency_key(task.task.run.id, task.task.seq);
        let delay = self.config.backoff.delay(&call, task.attempt).max(at_least);
        let next_attempt_at = self.clock.now().saturating_add(delay);
        Ok(self.journal.retry_tool_task(token, next_attempt_at).await?)
    }

    /// The call's args, loaded from where they live.
    pub(super) async fn task_args(&self, task: &ToolTask) -> Result<JsonValue, EngineError> {
        Ok(self.load_payload(&task.task.run.tenant, &task.args).await?)
    }

    /// The idempotency key every attempt of the call carries: `"{run_id}/{seq}"`, or its
    /// parent's key for a call a fork inherited without an outcome.
    pub(super) async fn task_key(
        &self,
        record: &RunRecord,
        task: &ToolTask,
    ) -> Result<String, EngineError> {
        self.call_key(record, task.task.seq, &task.tool, &task.args)
            .await
    }

    /// The trace context the call forwards: the run's trace, with the tool call span as parent,
    /// so that the tool's own spans join it.
    pub(super) fn call_trace_context(&self, record: &RunRecord, seq: Seq) -> Option<TraceContext> {
        Some(crate::telemetry::tool_call_trace_context(
            record,
            seq,
            &self.config.telemetry,
        ))
    }

    /// A result above the payload limit fails the call instead of reaching the script.
    pub(super) fn bounded_result(&self, result: JsonValue) -> Result<JsonValue, EngineError> {
        let max = self.config.limits.max_payload_bytes;
        let payload = Payload::Value(result);
        let size = payloads::value_size(&payload).unwrap_or_default();
        match payload {
            Payload::Value(result) if size <= max => Ok(result),
            _ => Err(EngineError::ResultTooLarge { size, max }),
        }
    }

    /// The deadline of a deferred task: its execution timeout from now, bounded by the run's
    /// deadline (and by `requested`, when the handler asks for less).
    pub(super) fn deferral_deadline(
        &self,
        task: &ToolTask,
        record: &RunRecord,
        requested: Option<Duration>,
    ) -> Timestamp {
        let now = self.clock.now();
        [requested, task.policy.execution_timeout]
            .into_iter()
            .flatten()
            .map(|timeout| now.saturating_add(timeout))
            .fold(record.expires_at, Timestamp::min)
    }

    // ---------------------------------------------------------------------------------------
    // The dispatched path

    /// The call as the dispatcher receives it, with its authentication. `None` when the run has
    /// ended.
    async fn prepare(
        &self,
        task: &ToolTask,
        token: &TaskToken,
    ) -> Result<Option<Prepared>, EngineError> {
        let run = &task.task.run;
        let Some(record) = self.journal.get_run(run).await? else {
            return Ok(None);
        };
        if record.status.is_terminal() {
            return Ok(None);
        }
        let stored = self.toolset(&run.tenant, task.toolset).await?;
        let tool =
            stored
                .toolset
                .tool(&task.tool)
                .cloned()
                .ok_or_else(|| EngineError::UnknownTool {
                    run_id: run.id,
                    tool: task.tool.clone(),
                })?;
        let (server, api) = match &tool.route {
            ToolRoute::Mcp { server } => (stored.toolset.server(server).cloned(), None),
            ToolRoute::Api { api } => (None, stored.toolset.api(api).cloned()),
            ToolRoute::Builtin(_) | ToolRoute::Host { .. } => (None, None),
        };
        let auth = server
            .as_ref()
            .map(|server| server.auth.clone())
            .or_else(|| api.as_ref().map(|api| api.auth.clone()))
            .unwrap_or_default();
        let call = ToolCall {
            run: run.clone(),
            namespace: record.namespace.clone(),
            labels: record.metadata.labels.clone(),
            seq: task.task.seq,
            tool,
            server,
            api,
            args: self.task_args(task).await?,
            idempotency_key: self.task_key(&record, task).await?,
            attempt: task.attempt,
            schema_change_approved: task.schema_change_approved,
            token: token.clone(),
            trace_context: self.call_trace_context(&record, task.task.seq),
        };
        let served = self.served(task, &call).await?;
        let auth = self.call_auth(&record, &call, &auth).await?;
        Ok(Some(Prepared {
            record,
            call,
            auth,
            served,
        }))
    }

    /// How a call proves who calls, from its server's or API's `auth` (`archi/04-tools.md`,
    /// "Authenticating calls"): nothing; the secret the run attached for the server, read in the
    /// run's namespace and counted; or the tenant's signing key with the claims of the call.
    /// Nothing falls back on another secret. Mock mode reads no secret and signs nothing
    /// (`archi/05-testing-and-observability.md`): the mocks answer.
    async fn call_auth(
        &self,
        record: &RunRecord,
        call: &ToolCall,
        auth: &ToolAuth,
    ) -> Result<Auth, EngineError> {
        if *auth == ToolAuth::None || self.run_mode(&call.run).await? != Some(RunMode::Live) {
            return Ok(Auth::Ready(CallAuth::None));
        }
        self.live_call_auth(record, call, auth).await
    }

    async fn live_call_auth(
        &self,
        record: &RunRecord,
        call: &ToolCall,
        auth: &ToolAuth,
    ) -> Result<Auth, EngineError> {
        let tenant = &call.run.tenant;
        let server = call.tool.route.server().unwrap_or(&call.tool.name);
        let refused = |message: String| {
            Ok(Auth::Refused(ToolFailure {
                kind: ToolFailureKind::Unavailable,
                message,
                data: None,
            }))
        };
        match auth {
            ToolAuth::None => Ok(Auth::Ready(CallAuth::None)),
            ToolAuth::Secret(placement) => {
                // Checked at start: a run that imports the server attaches a secret for it.
                let Some(attached) = record.metadata.secrets.get(server) else {
                    return refused(format!(
                        "{server} needs a secret, and the run attached none for it: fork the run \
                         with \"secrets\": {{\"{server}\": \"org/<name>\" or \"ns/<name>\"}}"
                    ));
                };
                let id = SecretId::resolve(attached, &record.namespace);
                let reader = SecretReader {
                    run: call.run.id,
                    namespace: record.namespace.clone(),
                };
                let read = self
                    .secrets
                    .read(tenant, &id, &reader, self.clock.now())
                    .await?;
                Ok(match read {
                    Some(value) => Auth::Ready(CallAuth::Secret {
                        value,
                        placement: placement.clone(),
                    }),
                    None => Auth::SecretMissing {
                        server: server.to_owned(),
                        secret: attached.clone(),
                    },
                })
            }
            ToolAuth::Rewind => {
                let Some(public_url) = &self.config.public_url else {
                    return refused(format!(
                        "{server} checks Rewind's signature, and this server has no public URL \
                         where its public keys are published (--public-url)"
                    ));
                };
                let key = self.secrets.signing_key(tenant, self.clock.now()).await?;
                let claims = CallClaims {
                    issuer: CallClaims::issuer(public_url, tenant),
                    subject: record.namespace.clone(),
                    labels: record.metadata.labels.clone(),
                    run: call.run.id,
                    tool: call.tool.name.clone(),
                    idempotency_key: call.idempotency_key.clone(),
                    seq: call.seq,
                    attempt: call.attempt,
                };
                Ok(Auth::Ready(CallAuth::Signed { key, claims }))
            }
        }
    }

    /// The mode the run started in, from its `run_started`; `None` before its first activation.
    /// Read once per run on this engine: it never changes.
    async fn run_mode(&self, run: &RunRef) -> Result<Option<RunMode>, EngineError> {
        if let Some(mode) = self.tool_tasks.mode(run) {
            return Ok(Some(mode));
        }
        let history = self.journal.load_history(run, 0).await?;
        let mode = history.first().and_then(|entry| match &entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(started)) => Some(started.mode),
            _ => None,
        });
        if let Some(mode) = mode {
            self.tool_tasks.remember_mode(run, mode);
        }
        Ok(mode)
    }

    /// Whether the engine serves the call itself: the `webhook` built-in and the workflows'
    /// built-ins of a live run, which need the store. In mock mode, the mocks answer them like
    /// every other call, and the engine turns a workflow's start into a start and an end.
    async fn served(
        &self,
        task: &ToolTask,
        call: &ToolCall,
    ) -> Result<Option<Served>, EngineError> {
        let served = match &call.tool.route {
            ToolRoute::Builtin(Builtin::Webhook) => Served::Webhook,
            ToolRoute::Builtin(Builtin::Workflow { workflow, op }) => Served::Workflow {
                workflow: workflow.clone(),
                op: *op,
            },
            _ => return Ok(None),
        };
        let mode = self.run_mode(&task.task.run).await?;
        Ok(match (mode, served) {
            (Some(RunMode::Live), served) => Some(served),
            (
                _,
                Served::Workflow {
                    op: WorkflowOp::Start,
                    ..
                },
            ) => Some(Served::MockStart),
            _ => None,
        })
    }

    /// `webhook(name)`: a URL bound to the run and the signal name, the same for the same
    /// name. Requests posted to it become `name` signals (`archi/01-product.md`, "Signals").
    async fn webhook_builtin(&self, call: &ToolCall) -> Dispatched {
        let fail = |kind, message: &str| {
            Dispatched::Failed(ToolFailure {
                kind,
                message: message.to_owned(),
                data: None,
            })
        };
        let Some(base) = &self.config.public_url else {
            return fail(
                ToolFailureKind::Unavailable,
                "this server has no public URL, so it cannot receive webhooks",
            );
        };
        let Some(name) = call.args.get("name").and_then(JsonValue::as_str) else {
            return fail(
                ToolFailureKind::Error,
                r#"webhook expects {"name": string}"#,
            );
        };
        if let Err(refused) = text::refuse_nul("the webhook name", name) {
            return fail(ToolFailureKind::Error, &refused.to_string());
        }
        match self.journal.create_webhook_token(&call.run, name).await {
            Ok(token) => Dispatched::Result(json!({
                "url": format!("{}/hooks/{}", base.trim_end_matches('/'), token.0),
            })),
            Err(error) => Dispatched::Transient(ToolFailure {
                kind: ToolFailureKind::Unavailable,
                message: format!("the webhook could not be created: {error}"),
                data: None,
            }),
        }
    }

    /// Reports what became of an attempt to the store.
    async fn settle(
        &self,
        task: &ToolTask,
        token: &TaskToken,
        record: &RunRecord,
        dispatched: Dispatched,
    ) -> Result<Option<TaskProgress>, EngineError> {
        let waiting = |phase| {
            Some(TaskProgress {
                seq: task.task.seq,
                attempt: task.attempt,
                phase,
            })
        };
        match dispatched {
            Dispatched::Result(result) => {
                let outcome = match self.bounded_result(result) {
                    Ok(result) => ToolOutcome::Result(result),
                    Err(error) => ToolOutcome::Failure(ToolFailure {
                        kind: ToolFailureKind::Error,
                        message: error.to_string(),
                        data: None,
                    }),
                };
                self.finish(task, token, outcome).await?;
            }
            Dispatched::Failed(failure) => {
                self.finish(task, token, ToolOutcome::Failure(failure))
                    .await?;
            }
            Dispatched::Transient(failure) => {
                let retry = task.policy.on_interruption == Interruption::Retry
                    && task.attempt < self.config.max_tool_attempts;
                if retry {
                    metrics::tool_retry(&task.tool);
                    let asked = failure
                        .data
                        .as_ref()
                        .and_then(|data| data.get(RETRY_AFTER_DATA))
                        .and_then(serde_json::Value::as_u64)
                        .map_or(Duration::ZERO, Duration::from_secs);
                    self.retry_after(task, token, asked).await?;
                } else {
                    self.finish(task, token, ToolOutcome::Failure(failure))
                        .await?;
                }
            }
            Dispatched::Deferred => {
                let until = self.deferral_deadline(task, record, None);
                if self.journal.defer_tool_task(token, until).await? == TaskUpdate::Applied {
                    return Ok(waiting(TaskPhase::Deferred));
                }
            }
            Dispatched::Park(reason) => {
                if self.journal.park_tool_task(token, reason.clone()).await? == TaskUpdate::Applied
                {
                    self.report_parked(task, record, reason).await?;
                    return Ok(waiting(TaskPhase::Parked));
                }
            }
        }
        Ok(None)
    }

    /// Tells the tenant's subscriptions that a call waits for the harness. Sent once the task
    /// is parked, so that an approval sent in response finds it parked. A delivery that fails
    /// after the sender's retries is logged: the run's state still shows the parked call.
    async fn report_parked(
        &self,
        task: &ToolTask,
        record: &RunRecord,
        reason: ParkReason,
    ) -> Result<(), EngineError> {
        let (seq, tool, message) = (task.task.seq, task.tool.clone(), reason.message);
        let credentials_expired = |reason| WebhookEventKind::CredentialsExpired {
            seq,
            tool: tool.clone(),
            message: message.clone(),
            reason,
            secret: task
                .route
                .server()
                .and_then(|server| record.metadata.secrets.get(server))
                .cloned(),
        };
        let kind = match reason.kind {
            ParkKind::SchemaChanged => WebhookEventKind::ToolSchemaChanged {
                seq,
                tool: tool.clone(),
                message: message.clone(),
            },
            ParkKind::CredentialsRefused => credentials_expired(CredentialsExpiredReason::Refused),
            ParkKind::SecretMissing => credentials_expired(CredentialsExpiredReason::Missing),
        };
        let run = &task.task.run;
        let event = WebhookEvent {
            id: format!("{}/{}/{}", run.id, seq.0, task.attempt),
            tenant: run.tenant.clone(),
            namespace: Some(record.namespace.clone()),
            run: Some(run.id),
            labels: record.metadata.labels.clone(),
            occurred_at: self.clock.now(),
            kind,
        };
        tracing::warn!(task = %task.task, event = event.kind.event_type().as_str(), "tool call parked");
        for subscription in self.journal.subscriptions(&run.tenant).await? {
            if !subscription.accepts(&event) {
                continue;
            }
            if let Err(error) = self.webhooks.send(&subscription, &event).await {
                tracing::error!(task = %task.task, %error, "webhook not delivered");
            }
        }
        Ok(())
    }
}

/// The shortest secret whose value is taken out of answers: a shorter one would match ordinary
/// text.
const MIN_REDACTED_SECRET: usize = 4;

/// What replaces a secret's value in an answer.
const REDACTED: &str = "[secret]";

/// The texts of the attached secret an answer must not carry: its value, and its base64 form
/// when its header encodes it (HTTP basic).
fn secret_texts(auth: &CallAuth) -> Vec<String> {
    let CallAuth::Secret { value, placement } = auth else {
        return Vec::new();
    };
    let value = value.expose();
    if value.len() < MIN_REDACTED_SECRET {
        return Vec::new();
    }
    let mut texts = vec![value.to_owned()];
    if let SecretPlacement::Header {
        encoding: SecretEncoding::Base64,
        ..
    } = placement
    {
        use base64::Engine as _;
        texts.push(base64::engine::general_purpose::STANDARD.encode(value));
    }
    texts
}

/// `dispatched` without the attached secret's texts: a server that echoes its credential (an
/// error quoting the key, a debug endpoint) must not bring it into the journal and the script
/// (`archi/04-tools.md`, "Secrets": secrets never enter the journal nor the isolate).
fn redacted(dispatched: Dispatched, texts: &[String]) -> Dispatched {
    if texts.is_empty() {
        return dispatched;
    }
    let failure = |mut failure: ToolFailure| {
        failure.message = redacted_text(failure.message, texts);
        failure.data = failure.data.map(|data| redacted_json(data, texts));
        failure
    };
    match dispatched {
        Dispatched::Result(value) => Dispatched::Result(redacted_json(value, texts)),
        Dispatched::Failed(failed) => Dispatched::Failed(failure(failed)),
        Dispatched::Transient(failed) => Dispatched::Transient(failure(failed)),
        Dispatched::Park(mut reason) => {
            reason.message = redacted_text(reason.message, texts);
            Dispatched::Park(reason)
        }
        Dispatched::Deferred => Dispatched::Deferred,
    }
}

fn redacted_text(text: String, texts: &[String]) -> String {
    if texts.iter().any(|secret| text.contains(secret.as_str())) {
        texts
            .iter()
            .fold(text, |text, secret| text.replace(secret.as_str(), REDACTED))
    } else {
        text
    }
}

fn redacted_json(value: JsonValue, texts: &[String]) -> JsonValue {
    match value {
        JsonValue::String(text) => JsonValue::String(redacted_text(text, texts)),
        JsonValue::Array(items) => JsonValue::Array(
            items
                .into_iter()
                .map(|item| redacted_json(item, texts))
                .collect(),
        ),
        JsonValue::Object(fields) => JsonValue::Object(
            fields
                .into_iter()
                .map(|(key, value)| (redacted_text(key, texts), redacted_json(value, texts)))
                .collect(),
        ),
        other => other,
    }
}
