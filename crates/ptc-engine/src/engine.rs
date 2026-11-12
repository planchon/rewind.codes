//! The engine and its use cases: start, signal, cancel, activate, fire timers, expire runs,
//! quarantine, replay. The tool tasks' use cases are in [`crate::tasks`].

use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use ptc_core::{
    AbortKind, AbortReason, CancelReason, Completion, Digest, Epoch, Event, HistoryEntry,
    HistoryError, JsonValue, Lifecycle, NonDeterminism, Payload, RunId, RunMode, RunStarted,
    RunState, RunStatus, Script, Seq, Toolset,
};

use crate::adapters::StaticTypeChecker;
use crate::cache::{CacheConfig, InstanceCache};
use crate::payloads::{self, PayloadTiers};
use crate::ports::{
    Activation, BlobError, BlobStore, CheckerError, Clock, CreateRun, Created, Credentials,
    Diagnostic, Enqueued, Entropy, ExecutorError, InvalidScript, JournalStore, NewRun,
    QuarantineKind, RunFilter, RunMetadata, RunRecord, RunRef, ScriptExecutor, SecretError,
    SecretOwner, SecretStore, Severity, StoreError, StoredScript, StoredToolset, TaskFilter,
    TenantId, TenantScope, ToolDeclarations, ToolDispatcher, Trigger, TypeChecker, WebhookSender,
};
use crate::replay::{self, ReplayEnd, ReplayError, ReplayInput};
use crate::tasks::{Backoff, ToolTasks};
use crate::telemetry::TelemetryConfig;
use crate::text::{self, NulCharacter};

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
    /// Attempts at a tool call, every claim counted (`archi/04-tools.md`, "Policies"). A call
    /// whose claim expired that many times fails with `outcome_unknown`; a transient failure at
    /// the last attempt is delivered to the script.
    pub max_tool_attempts: u32,
    /// The tenants this worker serves.
    pub tenants: TenantScope,
    /// The tool tasks this worker claims and dispatches. `TaskFilter::All` by default: the
    /// engine dispatches every task, host tools included, as in mock mode.
    pub tool_tasks: TaskFilter,
    /// The delay before a tool call that failed transiently is attempted again.
    pub backoff: Backoff,
    /// The server's public URL. `webhook(name)` returns `{public_url}/hooks/{token}`; without
    /// it, the `webhook` built-in fails with `unavailable`.
    pub public_url: Option<String>,
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
            backoff: Backoff::default(),
            public_url: None,
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
pub struct Adapters<J, B, X, D, C, E, S, W> {
    pub journal: J,
    pub blobs: B,
    pub executor: X,
    pub dispatcher: D,
    pub clock: C,
    pub entropy: E,
    /// Credentials, resolved when a tool call is dispatched.
    pub secrets: S,
    /// Webhooks about tool tasks that wait for the harness (`tool.schema_changed`,
    /// `credentials.expired`).
    pub webhooks: W,
}

/// The application layer of ai-ptc. It is deterministic given its ports: wired with
/// in-memory adapters and a virtual clock, it is both mock mode and the test harness.
#[derive(Debug)]
pub struct Engine<J, B, X: ScriptExecutor, D, C, E, S, W> {
    pub(crate) journal: J,
    pub(crate) blobs: B,
    pub(crate) executor: X,
    pub(crate) dispatcher: D,
    pub(crate) clock: C,
    pub(crate) entropy: E,
    pub(crate) secrets: S,
    pub(crate) webhooks: W,
    /// The calls this engine is dispatching, and when its retried calls are due.
    pub(crate) tool_tasks: ToolTasks,
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
    /// The run's own credentials, stored before the run exists so that its first call uses
    /// them. They override the toolset's.
    pub credentials: Option<Credentials>,
}

impl StartRun {
    /// A live run of the tenant `local`, with no declarations, TTL, metadata, idempotency key
    /// or credentials.
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
            credentials: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum StartError {
    /// The script cannot run: it does not compile, imports what does not exist, has no
    /// default export… The message is written to be read by the model that wrote the script.
    #[error("the script is invalid: {0}")]
    InvalidScript(InvalidScript),
    /// The script does not type-check against the sandbox lib and the toolset's declarations:
    /// at least one diagnostic is an error. Every diagnostic is given, warnings included.
    #[error("the script does not type-check: {}", first_error(.0))]
    TypeErrors(Vec<Diagnostic>),
    /// The type-checker itself failed: the script was neither accepted nor refused.
    #[error(transparent)]
    Checker(#[from] CheckerError),
    #[error("invalid TTL {requested:?}: it must be positive and at most {max:?}")]
    InvalidTtl { requested: Duration, max: Duration },
    #[error("the input is {size} bytes as canonical JSON, above the limit of {max} bytes")]
    InputTooLarge { size: usize, max: usize },
    /// A text to store (the script, a label, the idempotency key, the trace context, the
    /// toolset's declarations) holds a NUL character.
    #[error(transparent)]
    NulCharacter(#[from] NulCharacter),
    #[error("the executor supports no determinism epoch")]
    NoEpoch,
    /// The tenant already has its maximum of active runs.
    #[error("the tenant already has {quota} active runs, its maximum")]
    QuotaExceeded { quota: u32 },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Secret(#[from] SecretError),
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
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// No such tool task for this tenant: it was answered, its run ended, or its claim was
    /// taken over by another worker. An outcome reported for it is dropped.
    #[error(
        "the task no longer exists: it was answered, its run ended, or its claim was taken over"
    )]
    TaskGone,
    /// Only a task waiting for the harness (`approval` or `parked`) can be approved or rejected.
    #[error("task {task} is {state}: only a task waiting for the harness is approved or rejected")]
    TaskNotWaiting { task: String, state: &'static str },
    /// A tool result above the engine's payload limit.
    #[error("the result is {size} bytes as canonical JSON, above the limit of {max} bytes")]
    ResultTooLarge { size: usize, max: usize },
    /// A text to store (a signal's idempotency key) holds a NUL character.
    #[error(transparent)]
    NulCharacter(#[from] NulCharacter),
    #[error("run {0} has ended")]
    RunEnded(RunId),
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
    pub fn new(adapters: Adapters<J, B, X, D, C, E, S, W>, config: EngineConfig) -> Self {
        let Adapters {
            journal,
            blobs,
            executor,
            dispatcher,
            clock,
            entropy,
            secrets,
            webhooks,
        } = adapters;
        Self {
            journal,
            blobs,
            executor,
            dispatcher,
            clock,
            entropy,
            secrets,
            webhooks,
            tool_tasks: ToolTasks::default(),
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

    pub fn secrets(&self) -> &S {
        &self.secrets
    }

    pub fn webhooks(&self) -> &W {
        &self.webhooks
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
        self.start(request, None::<&StaticTypeChecker>).await
    }

    /// [`start_run`](Self::start_run), with the script type-checked first against the sandbox
    /// lib and the toolset's declarations (`request.declarations`), as `ptc check` does: a
    /// script with a type error is refused ([`StartError::TypeErrors`]) before any run exists
    /// (`archi/03-execution.md`, "Starting a run"). The checks at start come first: a script that
    /// does not even compile is refused with them.
    pub async fn start_run_checked<K: TypeChecker>(
        &self,
        request: StartRun,
        checker: &K,
    ) -> Result<Created, StartError> {
        self.start(request, Some(checker)).await
    }

    async fn start<K: TypeChecker>(
        &self,
        request: StartRun,
        checker: Option<&K>,
    ) -> Result<Created, StartError> {
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
        text::refuse_nul("the script", request.script.source())?;
        text::refuse_nul(text::TOOLSET_TEXT, &request.declarations.typescript)?;
        text::refuse_nul(text::TOOLSET_TEXT, &request.declarations.prompt)?;
        refuse_nul_in_metadata(&request.metadata, request.idempotency_key.as_deref())?;
        let epoch = *self.executor.epochs().last().ok_or(StartError::NoEpoch)?;
        let compiled = self
            .executor
            .compile(&request.script, &request.toolset)
            .map_err(StartError::InvalidScript)?;
        if let Some(checker) = checker {
            type_check(checker, &request.script, &request.declarations).await?;
        }
        let tenant = request.tenant;
        self.journal
            .put_script(&tenant, &request.script, &compiled)
            .await?;
        self.journal
            .put_toolset(&tenant, &request.toolset, &request.declarations)
            .await?;

        let now = self.clock.now();
        let run_ref = RunRef::new(tenant, self.entropy.run_id());
        // Stored before the run exists: a worker may dispatch its first call at once.
        let owner = SecretOwner::Run(run_ref.id);
        if let Some(credentials) = &request.credentials {
            self.secrets
                .put(&run_ref.tenant, &owner, credentials)
                .await?;
        }
        let run = NewRun {
            run: run_ref.clone(),
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
        let created = self.journal.create_run(run, request.idempotency_key).await;
        let created = match created {
            Ok(CreateRun::Created(created)) if created.created => created,
            // The run was not created: no run owns the credentials stored for it.
            other => {
                if request.credentials.is_some() {
                    self.secrets.delete(&run_ref.tenant, &owner).await?;
                }
                match other? {
                    CreateRun::Created(existing) => existing,
                    CreateRun::QuotaExceeded { quota } => {
                        return Err(StartError::QuotaExceeded { quota });
                    }
                }
            }
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
        if let Some(key) = &idempotency_key {
            text::refuse_nul("the idempotency key", key)?;
        }
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
            let inbox = self.journal.take_inbox(&lease).await?;
            let mut events = Vec::with_capacity(2);
            // A run quarantined before its first commit (a fork whose parent is gone) has an
            // empty history: `run_started` is committed with `run_aborted`, so that the history
            // still starts with it.
            if history.is_empty() {
                let started = inbox.iter().find_map(|item| match &item.trigger {
                    Trigger::RunStarted(started) => Some(started.clone()),
                    Trigger::Completion(_) => None,
                });
                let started = started.ok_or_else(|| EngineError::CorruptInbox {
                    run_id,
                    message: "a run that has not started has no run_started".into(),
                })?;
                events.push(Event::Lifecycle(Lifecycle::RunStarted(started)));
            }
            events.push(Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
                kind: match quarantine.kind {
                    QuarantineKind::NonDeterminism => AbortKind::NonDeterminism,
                    QuarantineKind::Failures => AbortKind::Internal,
                },
                message: quarantine.reason,
            })));
            let mut entries: Vec<HistoryEntry> = (state.history_len()..)
                .zip(events)
                .map(|(index, event)| HistoryEntry {
                    index,
                    recorded_at,
                    event,
                })
                .collect();
            let payloads = payloads::offload(
                &self.journal,
                &self.blobs,
                &run.tenant,
                &mut entries,
                self.config.payloads,
                self.clock.now(),
            )
            .await?;
            let consumed = inbox.into_iter().map(|item| item.id).collect();
            let mut activation = Activation::new(entries, consumed, RunStatus::Aborted);
            activation.payloads = payloads;
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

/// Type-checks a script; refuses it when a diagnostic is an error.
pub(crate) async fn type_check<K: TypeChecker>(
    checker: &K,
    script: &Script,
    declarations: &ToolDeclarations,
) -> Result<(), StartError> {
    let diagnostics = checker.check(script, declarations).await?;
    if diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == Severity::Error)
    {
        return Err(StartError::TypeErrors(diagnostics));
    }
    Ok(())
}

/// The first error of a refused check, for the message of [`StartError::TypeErrors`].
fn first_error(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .find(|diagnostic| diagnostic.severity == Severity::Error)
        .map_or_else(String::new, ToString::to_string)
}

/// Refuses run metadata, or an idempotency key, holding a NUL character.
pub(crate) fn refuse_nul_in_metadata(
    metadata: &RunMetadata,
    idempotency_key: Option<&str>,
) -> Result<(), NulCharacter> {
    text::refuse_nul_in_labels(&metadata.labels)?;
    if let Some(trace) = &metadata.trace_context {
        text::refuse_nul_in_trace(trace)?;
    }
    idempotency_key.map_or(Ok(()), |key| text::refuse_nul("the idempotency key", key))
}

/// `"{run_id}/{seq}"`, the key every attempt of a tool call carries.
pub fn idempotency_key(run_id: RunId, seq: Seq) -> String {
    format!("{run_id}/{}", seq.0)
}
