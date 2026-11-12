//! The engine and its use cases: start, signal, cancel, activate, fire timers, expire runs,
//! dispatch tool calls, quarantine, replay.

use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use ptc_core::{
    AbortKind, AbortReason, CancelReason, Completion, Digest, Epoch, Event, HistoryEntry,
    HistoryError, Interruption, JsonValue, Lifecycle, NonDeterminism, Payload, RunId, RunMode,
    RunStarted, RunState, RunStatus, Script, Seq, ToolFailure, ToolFailureKind, ToolOutcome,
    Toolset,
};

use crate::cache::{CacheConfig, InstanceCache};
use crate::payloads::{self, PayloadTiers};
use crate::ports::{
    Activation, BlobError, BlobStore, ClaimedTask, Clock, CreateRun, Created, Dispatched, Enqueued,
    Entropy, ExecutorError, InvalidScript, JournalStore, NewRun, QuarantineKind, RunFilter,
    RunMetadata, RunRecord, RunRef, ScriptExecutor, StoreError, StoredScript, StoredToolset,
    TaskFilter, TenantId, TenantScope, ToolCall, ToolDeclarations, ToolDispatcher,
};
use crate::replay::{self, ReplayEnd, ReplayError, ReplayInput};
use crate::telemetry::{TelemetryConfig, tool_call_trace_context};

/// Resource limits enforced by the engine (`archi/03-execution.md`, "Resource limits"). CPU
/// time and heap size are enforced by the executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// Entries in a run's history.
    pub max_history_len: u64,
    /// Commands the script may emit in one activation.
    pub max_commands_per_activation: usize,
    /// Size of one payload the script emits (tool args, output), as canonical JSON.
    pub max_payload_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_history_len: 50_000,
            max_commands_per_activation: 10_000,
            max_payload_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineConfig {
    /// This worker's name, as the owner of its leases.
    pub worker: String,
    pub lease_duration: Duration,
    /// TTL of a run started without one.
    pub default_ttl: Duration,
    /// The largest TTL a run may ask for.
    pub max_ttl: Duration,
    /// Attempts at a tool call whose dispatcher keeps dying mid-call. Past it, the call fails
    /// with `outcome_unknown`.
    pub max_tool_attempts: u32,
    /// The tenants this worker serves.
    pub tenants: TenantScope,
    /// The tool tasks this worker claims and dispatches. `TaskFilter::All` by default: the
    /// engine dispatches every task, host tools included, as in mock mode.
    pub tool_tasks: TaskFilter,
    /// Timers fired, and runs expired, per call to the store.
    pub scheduler_batch: usize,
    pub limits: Limits,
    /// This worker's build. A worker of a new build retries each quarantined run of its epochs
    /// once ([`Engine::retry_quarantined`]).
    pub build: String,
    /// The bounds of the sticky instance cache.
    pub cache: CacheConfig,
    /// Where payloads go at commit, by size.
    pub payloads: PayloadTiers,
    /// How often an activation renews its lease, about 10 s: the lease outlives a worker that
    /// renews it, and a dead worker frees the run at most `lease_duration` after its last
    /// renewal.
    pub lease_renewal: Duration,
    /// Run telemetry: sampling, and the labels exported as span attributes.
    pub telemetry: TelemetryConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        const DAY: u64 = 86_400;
        Self {
            worker: "worker".into(),
            lease_duration: Duration::from_secs(30),
            default_ttl: Duration::from_secs(30 * DAY),
            max_ttl: Duration::from_secs(365 * DAY),
            max_tool_attempts: 5,
            tenants: TenantScope::All,
            tool_tasks: TaskFilter::All,
            scheduler_batch: 1_000,
            limits: Limits::default(),
            build: concat!("ptc-engine ", env!("CARGO_PKG_VERSION")).into(),
            cache: CacheConfig::default(),
            payloads: PayloadTiers::default(),
            lease_renewal: Duration::from_secs(10),
            telemetry: TelemetryConfig::default(),
        }
    }
}

/// The adapters an engine is wired with. Composition roots pick them.
#[derive(Debug)]
pub struct Adapters<J, B, X, D, C, E> {
    pub journal: J,
    pub blobs: B,
    pub executor: X,
    pub dispatcher: D,
    pub clock: C,
    pub entropy: E,
}

/// The application layer of ai-ptc. It is deterministic given its ports: wired with
/// in-memory adapters and a virtual clock, it is both mock mode and the test harness.
#[derive(Debug)]
pub struct Engine<J, B, X: ScriptExecutor, D, C, E> {
    pub(crate) journal: J,
    pub(crate) blobs: B,
    pub(crate) executor: X,
    pub(crate) dispatcher: D,
    pub(crate) clock: C,
    pub(crate) entropy: E,
    pub(crate) config: EngineConfig,
    /// Draws for the tenant `acquire_runnable` picks: consecutive draws go round the tenants
    /// with runnable work.
    pub(crate) draws: AtomicU64,
    /// The instances of sticky execution, kept between activations.
    pub(crate) instances: Mutex<InstanceCache<X::Instance>>,
}

/// A request to start a run.
#[derive(Debug, Clone)]
pub struct StartRun {
    pub tenant: TenantId,
    pub script: Script,
    pub toolset: Toolset,
    /// The declarations generated from the toolset, stored with it.
    pub declarations: ToolDeclarations,
    pub input: JsonValue,
    pub mode: RunMode,
    /// The run's TTL; [`EngineConfig::default_ttl`] when `None`.
    pub ttl: Option<Duration>,
    pub metadata: RunMetadata,
    /// A retried request with the same key returns the same run instead of starting another.
    pub idempotency_key: Option<String>,
}

impl StartRun {
    /// A live run of the tenant `local`, with no declarations, TTL, metadata or idempotency
    /// key.
    pub fn new(script: Script, toolset: Toolset, input: JsonValue) -> Self {
        Self {
            tenant: TenantId::local(),
            script,
            toolset,
            declarations: ToolDeclarations::default(),
            input,
            mode: RunMode::Live,
            ttl: None,
            metadata: RunMetadata::default(),
            idempotency_key: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StartError {
    /// The script cannot run: it does not compile, imports what does not exist, has no
    /// default export… The message is written to be read by the model that wrote the script.
    #[error("the script is invalid: {0}")]
    InvalidScript(InvalidScript),
    #[error("invalid TTL {requested:?}: it must be positive and at most {max:?}")]
    InvalidTtl { requested: Duration, max: Duration },
    #[error("the input is {size} bytes as canonical JSON, above the limit of {max} bytes")]
    InputTooLarge { size: usize, max: usize },
    #[error("the executor supports no determinism epoch")]
    NoEpoch,
    /// The tenant already has its maximum of active runs.
    #[error("the tenant already has {quota} active runs, its maximum")]
    QuotaExceeded { quota: u32 },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    #[error("run {0} not found")]
    RunNotFound(RunId),
    /// A command names a tool its run's toolset does not have.
    #[error("run {run_id} called {tool}, which its toolset does not have")]
    UnknownTool { run_id: RunId, tool: String },
    #[error("script {0} not found")]
    ScriptNotFound(Digest),
    #[error("toolset {0} not found")]
    ToolsetNotFound(Digest),
    #[error("the history of run {run_id} is invalid: {error}")]
    CorruptHistory { run_id: RunId, error: HistoryError },
    #[error("the inbox of run {run_id} is invalid: {message}")]
    CorruptInbox { run_id: RunId, message: String },
    /// The executor could not rebuild the run, for a reason that may be transient (a limit
    /// hit under load, an internal error): the activation is retried later.
    #[error("the replay of run {run_id} could not go on: {error}")]
    ReplayInterrupted { run_id: RunId, error: ExecutorError },
}

/// What [`Engine::activate_next`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivationReport {
    pub run: RunRef,
    pub outcome: ActivationOutcome,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ActivationOutcome {
    /// Entries were committed; the run is now in `status`.
    Committed { entries: usize, status: RunStatus },
    /// Nothing was committed and the run is quarantined: replay did not match the history
    /// (`non_determinism`), or the activation failed for the fifth time in a row (`failures`).
    Quarantined {
        kind: QuarantineKind,
        reason: String,
    },
    /// The inbox was empty by the time the lease was acquired.
    NothingToDo,
    /// The activation failed and committed nothing. The lease is kept until it expires, then
    /// the run is activated again.
    Failed(EngineError),
}

/// The verdict of [`Engine::replay`].
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayVerdict {
    /// The script reproduces the history.
    Matches(ReplayEnd),
    /// The script diverges from the history, at the entry the error names.
    Diverges(NonDeterminism),
    /// The script could not go on where the original run went on.
    Fails(ExecutorError),
}

impl<J, B, X, D, C, E> Engine<J, B, X, D, C, E>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
{
    pub fn new(adapters: Adapters<J, B, X, D, C, E>, config: EngineConfig) -> Self {
        let Adapters {
            journal,
            blobs,
            executor,
            dispatcher,
            clock,
            entropy,
        } = adapters;
        Self {
            journal,
            blobs,
            executor,
            dispatcher,
            clock,
            entropy,
            instances: Mutex::new(InstanceCache::new(config.cache)),
            config,
            draws: AtomicU64::new(0),
        }
    }

    pub fn journal(&self) -> &J {
        &self.journal
    }

    pub fn blobs(&self) -> &B {
        &self.blobs
    }

    pub fn executor(&self) -> &X {
        &self.executor
    }

    pub fn dispatcher(&self) -> &D {
        &self.dispatcher
    }

    pub fn clock(&self) -> &C {
        &self.clock
    }

    pub fn config(&self) -> &EngineConfig {
        &self.config
    }

    // ---------------------------------------------------------------------------------------
    // The way in: start, signal, cancel

    /// Checks and compiles the script, stores it with the toolset, and creates the run. Its
    /// `run_started` is only enqueued: the first activation commits it with the first commands.
    ///
    /// A script that cannot run is refused here, before a run exists: the caller (the agent
    /// that wrote it) learns at once instead of at the first activation
    /// (`archi/03-execution.md`, "Starting a run").
    pub async fn start_run(&self, request: StartRun) -> Result<Created, StartError> {
        let ttl = request.ttl.unwrap_or(self.config.default_ttl);
        if ttl.is_zero() || ttl > self.config.max_ttl {
            return Err(StartError::InvalidTtl {
                requested: ttl,
                max: self.config.max_ttl,
            });
        }
        let input = Payload::Value(request.input);
        let size = payloads::value_size(&input).unwrap_or_default();
        if size > self.config.limits.max_payload_bytes {
            return Err(StartError::InputTooLarge {
                size,
                max: self.config.limits.max_payload_bytes,
            });
        }
        let epoch = *self.executor.epochs().last().ok_or(StartError::NoEpoch)?;
        let compiled = self
            .executor
            .compile(&request.script, &request.toolset)
            .map_err(StartError::InvalidScript)?;
        let tenant = request.tenant;
        self.journal
            .put_script(&tenant, &request.script, &compiled)
            .await?;
        self.journal
            .put_toolset(&tenant, &request.toolset, &request.declarations)
            .await?;

        let now = self.clock.now();
        let run = NewRun {
            run: RunRef::new(tenant, self.entropy.run_id()),
            started: RunStarted {
                script: request.script.digest(),
                toolset: request.toolset.digest(),
                input,
                seed: self.entropy.seed(),
                epoch,
                expires_at: now.saturating_add(ttl),
                mode: request.mode,
            },
            metadata: request.metadata,
            created_at: now,
        };
        let created = match self
            .journal
            .create_run(run, request.idempotency_key)
            .await?
        {
            CreateRun::Created(created) => created,
            CreateRun::QuotaExceeded { quota } => return Err(StartError::QuotaExceeded { quota }),
        };
        tracing::debug!(run = %created.run, created = created.created, "run started");
        Ok(created)
    }

    /// Sends a signal to a run. It lands in the run's mailbox for `name`, first in first out.
    pub async fn signal(
        &self,
        run: &RunRef,
        name: impl Into<String>,
        payload: JsonValue,
        idempotency_key: Option<String>,
    ) -> Result<Enqueued, EngineError> {
        let completion = Completion::SignalReceived {
            name: name.into(),
            payload: Payload::Value(payload),
        };
        self.enqueue(run, completion, idempotency_key).await
    }

    /// Asks a run to cancel: every pending await of the script rejects with `CancelledError`.
    pub async fn cancel(
        &self,
        run: &RunRef,
        message: Option<String>,
    ) -> Result<Enqueued, EngineError> {
        let completion = Completion::CancelRequested {
            reason: CancelReason::Requested(message),
        };
        self.enqueue(run, completion, None).await
    }

    async fn enqueue(
        &self,
        run: &RunRef,
        completion: Completion,
        idempotency_key: Option<String>,
    ) -> Result<Enqueued, EngineError> {
        Ok(self
            .journal
            .enqueue(run, completion, idempotency_key, self.clock.now())
            .await?)
    }

    // ---------------------------------------------------------------------------------------
    // Time: timers and deadlines

    /// Pushes `timer_fired` into the inbox of every run whose timer is due.
    pub async fn fire_due_timers(&self) -> Result<usize, EngineError> {
        Ok(self
            .journal
            .fire_due_timers(self.clock.now(), self.config.scheduler_batch)
            .await?)
    }

    /// Cancels the runs past their deadline (`cancel_requested { reason: "ttl" }`), and gives
    /// up the quarantined ones.
    pub async fn expire_due_runs(&self) -> Result<usize, EngineError> {
        let expired = self
            .journal
            .expire_due_runs(self.clock.now(), self.config.scheduler_batch)
            .await?;
        for run in &expired.quarantined {
            self.give_up(run).await?;
        }
        Ok(expired.cancelled.len() + expired.quarantined.len())
    }

    // ---------------------------------------------------------------------------------------
    // The way out: tool calls

    /// Claims tool tasks for this worker.
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

    /// Executes a claimed tool task and reports what became of it to the store.
    ///
    /// A task claimed again after its previous claim expired (its dispatcher died mid-call) is
    /// called again with the same idempotency key, so that the tool can tell it apart from a
    /// new call (`archi/04-tools.md`); under `at_most_once`, or after
    /// [`EngineConfig::max_tool_attempts`], the call fails with `outcome_unknown` instead.
    pub async fn run_tool_task(&self, claimed: ClaimedTask) -> Result<(), EngineError> {
        let ClaimedTask {
            task,
            token,
            previous_claim_expired,
        } = claimed;
        let run = task.task.run.clone();
        let seq = task.task.seq;
        let now = self.clock.now();
        let interrupted = task.attempt > self.config.max_tool_attempts
            || (previous_claim_expired && task.policy.on_interruption == Interruption::AtMostOnce);
        if interrupted {
            let failure = ToolFailure {
                kind: ToolFailureKind::OutcomeUnknown,
                message: format!(
                    "the call was interrupted {} times and may or may not have taken effect",
                    task.attempt - 1
                ),
                data: None,
            };
            self.journal
                .complete_tool_task(&token, ToolOutcome::Failure(failure), now)
                .await?;
            return Ok(());
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
        let server = tool
            .namespace()
            .and_then(|server| stored.toolset.server(server))
            .cloned();
        let record = self.run_record(&run).await?;
        let call = ToolCall {
            run: run.clone(),
            seq,
            tool,
            server,
            args: self.load_payload(&run.tenant, &task.args).await?,
            idempotency_key: self.call_key(&record, seq, &task.tool, &task.args).await?,
            attempt: task.attempt,
            token: token.clone(),
            trace_context: Some(tool_call_trace_context(
                &record,
                seq,
                &self.config.telemetry,
            )),
        };
        let journal = &self.journal;
        match self.dispatcher.dispatch(call, None).await {
            Dispatched::Result(result) => {
                let outcome = ToolOutcome::Result(result);
                journal
                    .complete_tool_task(&token, outcome, self.clock.now())
                    .await?;
            }
            Dispatched::Failed(failure) => {
                let outcome = ToolOutcome::Failure(failure);
                journal
                    .complete_tool_task(&token, outcome, self.clock.now())
                    .await?;
            }
            Dispatched::Transient(failure) => {
                let retry = task.policy.on_interruption == Interruption::Retry
                    && task.attempt < self.config.max_tool_attempts;
                if retry {
                    journal.retry_tool_task(&token, self.clock.now()).await?;
                } else {
                    let outcome = ToolOutcome::Failure(failure);
                    journal
                        .complete_tool_task(&token, outcome, self.clock.now())
                        .await?;
                }
            }
            Dispatched::Deferred => {
                journal.defer_tool_task(&token, record.expires_at).await?;
            }
            Dispatched::Park(reason) => {
                journal.park_tool_task(&token, reason).await?;
            }
        }
        Ok(())
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

    // ---------------------------------------------------------------------------------------
    // Quarantine

    /// Makes a quarantined run runnable again, typically after a fix. Returns whether it was
    /// quarantined.
    pub async fn release_quarantine(&self, run: &RunRef) -> Result<bool, EngineError> {
        Ok(self.journal.release_quarantine(run).await?)
    }

    /// Ends a quarantined run: `run_aborted { non_determinism }` after a replay mismatch,
    /// `run_aborted { internal }` after repeated failures. Returns `false` if the run is not
    /// quarantined, or is leased.
    pub async fn give_up(&self, run: &RunRef) -> Result<bool, EngineError> {
        let run_id = run.id;
        let lease = self
            .journal
            .acquire_run(
                run,
                &self.config.worker,
                self.clock.now(),
                self.config.lease_duration,
            )
            .await?;
        let Some(lease) = lease else {
            return Ok(false);
        };
        let result = async {
            let record = self.run_record(run).await?;
            let Some(quarantine) = record.quarantine else {
                return Ok(false);
            };
            let history = self.journal.load_history(run, 0).await?;
            let state = RunState::from_history(&history)
                .map_err(|error| EngineError::CorruptHistory { run_id, error })?;
            let recorded_at = state
                .updated_at()
                .map_or(self.clock.now(), |last| last.max(self.clock.now()));
            let entry = HistoryEntry {
                index: state.history_len(),
                recorded_at,
                event: Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
                    kind: match quarantine.kind {
                        QuarantineKind::NonDeterminism => AbortKind::NonDeterminism,
                        QuarantineKind::Failures => AbortKind::Internal,
                    },
                    message: quarantine.reason,
                })),
            };
            let consumed = self
                .journal
                .take_inbox(&lease)
                .await?
                .into_iter()
                .map(|item| item.id)
                .collect();
            let activation = Activation::new(vec![entry], consumed, RunStatus::Aborted);
            self.journal
                .commit_activation(&lease, activation, self.clock.now())
                .await?;
            tracing::warn!(%run_id, "quarantined run given up");
            Ok(true)
        }
        .await;
        self.journal.release(&lease).await?;
        result
    }

    // ---------------------------------------------------------------------------------------
    // Replay and queries

    /// Replays a run locally and checks determinism (`ptc replay`). With `script`, checks
    /// whether another script is compatible with the run's history. Nothing is written and no
    /// tool is called.
    pub async fn replay(
        &self,
        run: &RunRef,
        script: Option<&Script>,
    ) -> Result<ReplayVerdict, EngineError> {
        let run_id = run.id;
        let record = self.run_record(run).await?;
        let script = match script {
            Some(script) => script.clone(),
            None => self.stored_script(&run.tenant, record.script).await?.script,
        };
        let toolset = self.toolset(&run.tenant, record.toolset).await?.toolset;
        let compiled = self.executor.compile(&script, &toolset);
        let history = self.journal.load_history(run, 0).await?;
        let input = ReplayInput {
            run,
            compiled: compiled.as_ref(),
            toolset: &toolset,
            history: &history,
        };
        match replay::replay(&self.executor, &self.payloads(), input).await {
            Ok(rebuilt) => Ok(ReplayVerdict::Matches(rebuilt.end)),
            Err(ReplayError::NonDeterminism(mismatch)) => Ok(ReplayVerdict::Diverges(mismatch)),
            Err(ReplayError::Executor(err)) => Ok(ReplayVerdict::Fails(err)),
            Err(ReplayError::History(error)) => Err(EngineError::CorruptHistory { run_id, error }),
            Err(ReplayError::Blob(err)) => Err(err.into()),
            Err(ReplayError::LeaseLost(err)) => Err(err.into()),
        }
    }

    pub async fn get_run(&self, run: &RunRef) -> Result<Option<RunRecord>, EngineError> {
        Ok(self.journal.get_run(run).await?)
    }

    pub async fn list_runs(&self, filter: &RunFilter) -> Result<Vec<RunRecord>, EngineError> {
        Ok(self.journal.list_runs(filter).await?)
    }

    /// The run's history as stored: large payloads are digests.
    pub async fn history(&self, run: &RunRef) -> Result<Vec<HistoryEntry>, EngineError> {
        self.run_record(run).await?;
        Ok(self.journal.load_history(run, 0).await?)
    }

    /// The run's history with every payload loaded: a self-contained document.
    pub async fn export_history(&self, run: &RunRef) -> Result<Vec<HistoryEntry>, EngineError> {
        let mut history = self.history(run).await?;
        payloads::load_all(&self.payloads(), &run.tenant, &mut history).await?;
        Ok(history)
    }

    /// The run's state, folded from its history.
    pub async fn run_state(&self, run: &RunRef) -> Result<RunState, EngineError> {
        let history = self.history(run).await?;
        RunState::from_history(&history).map_err(|error| EngineError::CorruptHistory {
            run_id: run.id,
            error,
        })
    }

    pub(crate) async fn run_record(&self, run: &RunRef) -> Result<RunRecord, EngineError> {
        self.journal
            .get_run(run)
            .await?
            .ok_or(EngineError::RunNotFound(run.id))
    }

    pub(crate) async fn stored_script(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<StoredScript, EngineError> {
        self.journal
            .get_script(tenant, digest)
            .await?
            .ok_or(EngineError::ScriptNotFound(digest))
    }

    pub(crate) async fn toolset(
        &self,
        tenant: &TenantId,
        digest: Digest,
    ) -> Result<StoredToolset, EngineError> {
        self.journal
            .get_toolset(tenant, digest)
            .await?
            .ok_or(EngineError::ToolsetNotFound(digest))
    }

    /// The epochs this engine's executor runs.
    pub fn epochs(&self) -> &[Epoch] {
        self.executor.epochs()
    }
}

/// `"{run_id}/{seq}"`, the key every attempt of a tool call carries.
pub fn idempotency_key(run_id: RunId, seq: Seq) -> String {
    format!("{run_id}/{}", seq.0)
}
