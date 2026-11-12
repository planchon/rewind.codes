//! One activation (`archi/03-execution.md`, "Activations"; "Sticky execution").
//!
//! ```text
//! lease ← acquire_runnable                                # the sticky worker first
//! instance ← the cached instance if its history length is the run's (a hit),
//!            else a new instance + replay(history)        # a miss
//! for trigger in take_inbox(lease), in arrival order:
//!     recorded_at ← max(now, last recorded_at)            # monotonic per run
//!     if the run is terminal: drop the trigger
//!     append the trigger; deliver it; append what the script emitted
//! offload large payloads; derive tool tasks and timers from the new commands
//! commit_activation(lease, …)                             # one transaction, fenced
//! cache[run] ← instance                                   # evictions give up stickiness
//! ```
//!
//! The lease is renewed about every 10 s while this runs, between calls into the script, replay
//! included. A lost lease ends the activation without a commit.
//!
//! A trigger is always committed with everything the script emitted in response to it (the
//! commit invariant). When the host must end the run instead (the script cannot load, a limit
//! is exceeded, the cancellation grace runs out), the trigger is committed with `run_aborted`
//! and what the script emitted in response to it is dropped. A replay mismatch commits
//! nothing: the run is quarantined. Any other failure commits nothing and keeps the lease until
//! it expires; the fifth in a row quarantines the run.

use std::cmp::max;
use std::collections::BTreeSet;
use std::time::Duration;

use ptc_core::{
    AbortKind, AbortReason, Command, Completion, Event, HistoryEntry, HistoryError, Lifecycle,
    RunState, Timestamp, ToolRoute, Toolset,
};
use tracing::Instrument;

use crate::cache::Cached;
use crate::engine::{
    ActivationOutcome, ActivationReport, Adapters, Engine, EngineError, Limits,
};
use crate::payloads;
use crate::ports::{
    AcquireRunnable, Activation, BlobStore, Clock, Emitted, Entropy, ExecutorError, Failures,
    JournalStore, Lease, NewTimer, NewToolTask, QuarantineKind, RunRef, STICKY_GRACE,
    ScriptExecutor, ScriptInstance, StartScript, StoreError, ToolDispatcher, Trigger,
};
use crate::replay::{self, Heartbeat, Rebuilt, ReplayEnd, ReplayError, ReplayInput};
use crate::telemetry::{SpanKey, span_id, trace_of};

/// Why a run is activated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// It has something in its inbox.
    Runnable,
    /// It is quarantined, and a worker of a new build retries it once.
    QuarantineRetry,
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
    /// Leases one runnable run of a supported epoch and activates it. `None` when no run is
    /// runnable. An activation that fails is reported, not returned as an error: only a
    /// failure to lease a run is.
    pub async fn activate_next(&self) -> Result<Option<ActivationReport>, EngineError> {
        let request = AcquireRunnable {
            owner: &self.config.worker,
            epochs: self.executor.epochs(),
            tenants: &self.config.tenants,
            now: self.clock.now(),
            lease_duration: self.config.lease_duration,
            sticky_grace: STICKY_GRACE,
            draw: self
                .draws
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        };
        let Some(lease) = self.journal.acquire_runnable(&request).await? else {
            return Ok(None);
        };
        let outcome = self.activate_leased(&lease, Purpose::Runnable).await;
        Ok(Some(ActivationReport {
            run: lease.run,
            outcome,
        }))
    }

    /// Retries, once, each quarantined run of this worker's epochs that no worker of this build
    /// has retried yet: a worker of a new build calls it when it starts
    /// (`archi/03-execution.md`, "Non-determinism and quarantine"). A run whose replay matches
    /// again leaves quarantine and goes on; a replay that fails again commits nothing and
    /// leaves the run quarantined.
    pub async fn retry_quarantined(&self) -> Result<Vec<ActivationReport>, EngineError> {
        let mut reports = Vec::new();
        while let Some(lease) = self
            .journal
            .acquire_quarantined(
                &self.config.worker,
                &self.config.build,
                self.executor.epochs(),
                self.clock.now(),
                self.config.lease_duration,
            )
            .await?
        {
            let outcome = self.activate_leased(&lease, Purpose::QuarantineRetry).await;
            reports.push(ActivationReport {
                run: lease.run,
                outcome,
            });
        }
        Ok(reports)
    }

    /// The engine's adapters, to wire another engine on the same store: a worker restarted with
    /// a new configuration, such as a new build. The instance cache is dropped.
    pub fn into_adapters(self) -> Adapters<J, B, X, D, C, E> {
        Adapters {
            journal: self.journal,
            blobs: self.blobs,
            executor: self.executor,
            dispatcher: self.dispatcher,
            clock: self.clock,
            entropy: self.entropy,
        }
    }

    /// The number of instances this worker keeps for sticky execution.
    pub fn cached_instances(&self) -> usize {
        self.instances.lock().map_or(0, |cache| cache.len())
    }

    async fn activate_leased(&self, lease: &Lease, purpose: Purpose) -> ActivationOutcome {
        match self.activate(lease, purpose).await {
            Ok(outcome) => outcome,
            Err(error) => self.activation_failed(lease, purpose, error).await,
        }
    }

    /// Activates the leased run: rebuilds its instance, delivers its inbox, commits, and keeps
    /// the instance. On error nothing is committed, and the instance is dropped.
    pub(crate) async fn activate(
        &self,
        lease: &Lease,
        purpose: Purpose,
    ) -> Result<ActivationOutcome, EngineError> {
        let span = tracing::info_span!(
            "activation",
            run = %lease.run,
            fence = lease.fence,
            trace_id = tracing::field::Empty,
            span_id = tracing::field::Empty,
        );
        self.activate_in_span(lease, purpose)
            .instrument(span)
            .await
    }

    async fn activate_in_span(
        &self,
        lease: &Lease,
        purpose: Purpose,
    ) -> Result<ActivationOutcome, EngineError> {
        let run = &lease.run;
        let run_id = run.id;
        let tenant = &run.tenant;
        let mut heartbeat = LeaseHeartbeat {
            journal: &self.journal,
            clock: &self.clock,
            lease,
            every: self.config.lease_renewal,
            duration: self.config.lease_duration,
            last: self.clock.now(),
        };
        let inbox = self.journal.take_inbox(lease).await?;
        if inbox.is_empty() && purpose == Purpose::Runnable {
            self.journal.release(lease).await?;
            return Ok(ActivationOutcome::NothingToDo);
        }

        let record = self.run_record(run).await?;
        let (trace, _) = trace_of(&record);
        let current = tracing::Span::current();
        current.record("trace_id", tracing::field::display(trace));
        current.record(
            "span_id",
            tracing::field::display(span_id(run_id, SpanKey::Activation(lease.fence))),
        );
        let stored = self.stored_script(tenant, record.script).await?;
        let toolset = self.toolset(tenant, record.toolset).await?.toolset;

        // Sticky execution: a hit delivers the new triggers to the instance as it is.
        let cached = self.take_instance(run);
        let (mut instance, state) = match cached {
            Some(cached)
                if purpose == Purpose::Runnable
                    && cached.state.history_len() == record.history_len =>
            {
                tracing::debug!("instance cache hit");
                (Some(cached.instance), cached.state)
            }
            _ => {
                let history = self.journal.load_history(run, 0).await?;
                let state = RunState::from_history(&history)
                    .map_err(|error| EngineError::CorruptHistory { run_id, error })?;
                let mut instance = None;
                if !history.is_empty() && !state.is_terminal() {
                    let input = ReplayInput {
                        run,
                        compiled: Ok(&stored.compiled),
                        toolset: &toolset,
                        history: &history,
                    };
                    let rebuilt = replay::replay_with_heartbeat(
                        &self.executor,
                        &self.payloads(),
                        &mut heartbeat,
                        input,
                    )
                    .await;
                    match rebuilt {
                        Ok(Rebuilt {
                            instance: Some(rebuilt),
                            end: ReplayEnd::EndOfHistory,
                        }) => instance = Some(rebuilt),
                        Ok(_) => {
                            return Err(EngineError::CorruptInbox {
                                run_id,
                                message: "the replay of a run that has not ended stopped early"
                                    .into(),
                            });
                        }
                        // A script cannot be non-deterministic by construction: a mismatch is
                        // a fault of the platform, never of the script. The run waits for a
                        // fix.
                        Err(ReplayError::NonDeterminism(mismatch)) => {
                            return self.quarantine(lease, mismatch.to_string()).await;
                        }
                        // A script that loaded before and no longer does, or an epoch no
                        // longer supported, is a fault of the platform: the run waits for a
                        // fix.
                        Err(ReplayError::Executor(
                            err @ (ExecutorError::InvalidScript(_)
                            | ExecutorError::UnsupportedEpoch(_)),
                        )) => {
                            return self
                                .quarantine(lease, format!("the replay could not go on: {err}"))
                                .await;
                        }
                        // A limit hit under load, an internal error: worth retrying later.
                        Err(ReplayError::Executor(error)) => {
                            return Err(EngineError::ReplayInterrupted { run_id, error });
                        }
                        Err(ReplayError::History(error)) => {
                            return Err(EngineError::CorruptHistory { run_id, error });
                        }
                        Err(ReplayError::Blob(err)) => return Err(err.into()),
                        Err(ReplayError::LeaseLost(err)) => return Err(err.into()),
                    }
                }
                (instance, state)
            }
        };

        if purpose == Purpose::QuarantineRetry {
            // The replay matches again: the run leaves quarantine and goes on.
            self.journal.release_quarantine(run).await?;
            tracing::warn!("a quarantined run replays again: released");
            if inbox.is_empty() {
                self.journal.release(lease).await?;
                if let Some(instance) = instance {
                    self.keep_instance(run.clone(), Cached { instance, state })
                        .await;
                }
                return Ok(ActivationOutcome::NothingToDo);
            }
        }

        let last_recorded_at = state.updated_at().unwrap_or(Timestamp(0));
        let mut batch = Batch::new(run, state, last_recorded_at, &self.config.limits);
        for item in inbox {
            batch.consumed.push(item.id);
            if batch.state.is_terminal() {
                // A completion for a run that has ended: dropped.
                continue;
            }
            match item.trigger {
                Trigger::RunStarted(started) => {
                    if batch.state.history_len() != 0 {
                        return Err(EngineError::CorruptInbox {
                            run_id,
                            message: "run_started for a run that has already started".into(),
                        });
                    }
                    if let Some(parent) = record.metadata.parent {
                        let fork = crate::fork::ForkSource {
                            run,
                            parent,
                            started,
                            compiled: &stored.compiled,
                            toolset: &toolset,
                        };
                        instance = self
                            .start_fork(&mut batch, &mut heartbeat, fork)
                            .await?;
                        continue;
                    }
                    let recorded_at = batch.recorded_at(self.clock.now());
                    let input = self.load_payload(tenant, &started.input).await?;
                    let trigger = Event::Lifecycle(Lifecycle::RunStarted(started.clone()));
                    let start = StartScript {
                        run_id,
                        compiled: stored.compiled.clone(),
                        toolset: toolset.clone(),
                        input,
                        seed: started.seed,
                        epoch: started.epoch,
                        started_at: recorded_at,
                    };
                    heartbeat.beat().await?;
                    match self.executor.start(start).await {
                        Ok((started, emitted)) => {
                            instance = Some(started);
                            batch.respond(recorded_at, trigger, emitted, Grace::No)
                        }
                        Err(err) => batch.abort(recorded_at, trigger, abort_reason(err)),
                    }
                    .map_err(|error| EngineError::CorruptHistory { run_id, error })?;
                }
                Trigger::Completion(completion) => {
                    let Some(running) = instance.as_mut() else {
                        return Err(EngineError::CorruptInbox {
                            run_id,
                            message: format!("{} arrived before run_started", completion.name()),
                        });
                    };
                    let recorded_at = batch.recorded_at(self.clock.now());
                    self.deliver(&mut batch, &mut heartbeat, running, completion, recorded_at)
                        .await?;
                }
            }
        }

        heartbeat.beat().await?;
        let (mut activation, state) = batch.into_parts();
        let status = state.status();
        activation.payloads =
            payloads::offload(&self.blobs, tenant, &mut activation.entries, self.config.payloads)
                .await?;
        derive_effects(&mut activation, &state, &toolset)
            .map_err(|tool| EngineError::UnknownTool { run_id, tool })?;
        let entries = activation.entries.len();
        self.journal
            .commit_activation(lease, activation, self.clock.now())
            .await?;
        self.journal.release(lease).await?;
        if !state.is_terminal()
            && let Some(instance) = instance
        {
            self.keep_instance(run.clone(), Cached { instance, state })
                .await;
        }
        tracing::debug!(entries, status = %status, "activation committed");
        Ok(ActivationOutcome::Committed { entries, status })
    }

    /// Delivers one completion, recorded at `recorded_at`, and appends it with the script's
    /// response, or with `run_aborted` if the script cannot go on.
    pub(crate) async fn deliver<H: Heartbeat>(
        &self,
        batch: &mut Batch<'_>,
        heartbeat: &mut H,
        instance: &mut X::Instance,
        completion: Completion,
        recorded_at: Timestamp,
    ) -> Result<(), EngineError> {
        let run_id = batch.run_id;
        let tenant = batch.tenant.clone();
        let delivery = payloads::delivery(&self.payloads(), &tenant, &completion).await?;
        let grace = match completion {
            Completion::CancelRequested { .. } => Grace::Ends,
            _ => Grace::No,
        };
        let trigger = Event::Completion(completion);
        heartbeat.beat().await?;
        match instance.deliver(delivery, recorded_at).await {
            Ok(emitted) => batch.respond(recorded_at, trigger, emitted, grace),
            Err(err) => batch.abort(recorded_at, trigger, abort_reason(err)),
        }
        .map_err(|error| EngineError::CorruptHistory { run_id, error })
    }

    async fn quarantine(
        &self,
        lease: &Lease,
        mismatch: String,
    ) -> Result<ActivationOutcome, EngineError> {
        tracing::error!(%mismatch, "replay mismatch: run quarantined");
        self.journal
            .quarantine(lease, mismatch.clone(), self.clock.now())
            .await?;
        self.forget_instance(&lease.run).await;
        Ok(ActivationOutcome::Quarantined {
            kind: QuarantineKind::NonDeterminism,
            reason: mismatch,
        })
    }

    /// An activation failed and committed nothing. Unless the lease was lost, the failure is
    /// counted: the run is activated again once its lease expires, and the fifth failure in a
    /// row quarantines it. The retry of a quarantined run leaves it quarantined.
    async fn activation_failed(
        &self,
        lease: &Lease,
        purpose: Purpose,
        error: EngineError,
    ) -> ActivationOutcome {
        tracing::error!(run = %lease.run, %error, "activation failed");
        self.forget_instance(&lease.run).await;
        if matches!(error, EngineError::Store(StoreError::Fenced { .. })) {
            return ActivationOutcome::Failed(error);
        }
        if purpose == Purpose::QuarantineRetry {
            if let Err(err) = self.journal.release(lease).await {
                tracing::error!(run = %lease.run, error = %err, "the lease could not be released");
            }
            return ActivationOutcome::Failed(error);
        }
        let failures = self
            .journal
            .record_failure(lease, error.to_string(), self.clock.now())
            .await;
        match failures {
            Ok(Failures {
                count,
                quarantined: true,
            }) => {
                tracing::error!(
                    run = %lease.run,
                    count,
                    "the run's activations keep failing: run quarantined"
                );
                ActivationOutcome::Quarantined {
                    kind: QuarantineKind::Failures,
                    reason: error.to_string(),
                }
            }
            Ok(_) => ActivationOutcome::Failed(error),
            Err(err) => {
                tracing::error!(run = %lease.run, error = %err, "the failure could not be recorded");
                ActivationOutcome::Failed(error)
            }
        }
    }

    fn take_instance(&self, run: &RunRef) -> Option<Cached<X::Instance>> {
        self.instances.lock().ok()?.take(run)
    }

    /// Keeps an instance for the run's next activation. The runs whose instance was evicted
    /// give up their stickiness, so that another worker takes them at once.
    async fn keep_instance(&self, run: RunRef, cached: Cached<X::Instance>) {
        let evicted = match self.instances.lock() {
            Ok(mut cache) => cache.insert(run, cached),
            Err(_) => return,
        };
        for run in evicted {
            self.give_up_stickiness(&run).await;
        }
    }

    /// Drops the run's instance, if this worker keeps one, and its stickiness.
    pub(crate) async fn forget_instance(&self, run: &RunRef) {
        drop(self.take_instance(run));
        self.give_up_stickiness(run).await;
    }

    async fn give_up_stickiness(&self, run: &RunRef) {
        if let Err(err) = self.journal.forget_sticky(run, &self.config.worker).await {
            tracing::warn!(%run, error = %err, "the sticky priority could not be given up");
        }
    }
}

/// Renews a lease about every `every`, between the steps of an activation: each call into the
/// script is bounded by its CPU budget, so the steps are short. The engine has no timer of its
/// own: in virtual time, nothing is renewed while the clock does not move.
pub(crate) struct LeaseHeartbeat<'a, J, C> {
    pub(crate) journal: &'a J,
    pub(crate) clock: &'a C,
    pub(crate) lease: &'a Lease,
    pub(crate) every: Duration,
    pub(crate) duration: Duration,
    pub(crate) last: Timestamp,
}

impl<J: JournalStore, C: Clock> Heartbeat for LeaseHeartbeat<'_, J, C> {
    async fn beat(&mut self) -> Result<(), StoreError> {
        let now = self.clock.now();
        if now.saturating_since(self.last) < self.every {
            return Ok(());
        }
        self.journal.renew(self.lease, now, self.duration).await?;
        self.last = now;
        Ok(())
    }
}

/// Whether delivering a trigger ends the cancellation grace: after `cancel_requested`, the
/// script gets this one delivery to end by itself.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grace {
    No,
    Ends,
}

pub(crate) fn abort_reason(err: ExecutorError) -> AbortReason {
    let kind = match &err {
        ExecutorError::InvalidScript(_) => AbortKind::InvalidScript,
        ExecutorError::LimitExceeded(_) => AbortKind::LimitExceeded,
        ExecutorError::UnsupportedEpoch(_) | ExecutorError::Internal(_) => AbortKind::Internal,
    };
    let message = match err {
        ExecutorError::InvalidScript(message) | ExecutorError::LimitExceeded(message) => message,
        other => other.to_string(),
    };
    AbortReason { kind, message }
}

/// The entries of one activation, folded into the run's state as they are appended.
pub(crate) struct Batch<'l> {
    pub(crate) run_id: ptc_core::RunId,
    pub(crate) tenant: crate::ports::TenantId,
    pub(crate) state: RunState,
    last_recorded_at: Timestamp,
    entries: Vec<HistoryEntry>,
    pub(crate) consumed: Vec<u64>,
    commands: usize,
    /// The commands of entries copied from a fork's parent do not count against the limit per
    /// activation: they are bounded by the history's.
    pub(crate) copying: bool,
    limits: &'l Limits,
}

impl<'l> Batch<'l> {
    fn new(
        run: &RunRef,
        state: RunState,
        last_recorded_at: Timestamp,
        limits: &'l Limits,
    ) -> Self {
        Self {
            run_id: run.id,
            tenant: run.tenant.clone(),
            state,
            last_recorded_at,
            entries: Vec::new(),
            consumed: Vec::new(),
            commands: 0,
            copying: false,
            limits,
        }
    }

    /// An empty batch for a run whose history is empty.
    pub(crate) fn for_run(run: &RunRef, limits: &'l Limits) -> Self {
        Self::new(run, RunState::new(), Timestamp(0), limits)
    }

    /// `max(now, last recorded_at)`: monotonic per run.
    pub(crate) fn recorded_at(&mut self, now: Timestamp) -> Timestamp {
        self.last_recorded_at = max(now, self.last_recorded_at);
        self.last_recorded_at
    }

    /// The entries appended so far.
    pub(crate) fn entries(&self) -> &[HistoryEntry] {
        &self.entries
    }

    /// Appends a trigger and what the script emitted in response, or the trigger and
    /// `run_aborted` if the response breaks a limit or the cancellation grace.
    pub(crate) fn respond(
        &mut self,
        at: Timestamp,
        trigger: Event,
        emitted: Vec<Emitted>,
        grace: Grace,
    ) -> Result<(), HistoryError> {
        let response: Vec<Event> = emitted.into_iter().map(Event::from).collect();
        if let Some(reason) = self.exceeded_limit(&response) {
            return self.abort(at, trigger, reason);
        }

        let mut state = self.state.clone();
        let mut group = Vec::with_capacity(response.len() + 1);
        for event in std::iter::once(trigger.clone()).chain(response) {
            let entry = HistoryEntry {
                index: state.history_len(),
                recorded_at: at,
                event,
            };
            if let Err(err) = state.apply(&entry) {
                let reason = AbortReason {
                    kind: AbortKind::Internal,
                    message: format!("the executor emitted an invalid response: {err}"),
                };
                return self.abort(at, trigger, reason);
            }
            group.push(entry);
        }
        if grace == Grace::Ends && !state.is_terminal() {
            let reason = AbortReason {
                kind: AbortKind::Cancelled,
                message: "the script did not end within its cancellation grace".into(),
            };
            return self.abort(at, trigger, reason);
        }

        if !self.copying {
            self.commands += group
                .iter()
                .filter(|entry| matches!(entry.event, Event::Command(_)))
                .count();
        }
        self.state = state;
        self.entries.extend(group);
        Ok(())
    }

    /// Appends the trigger and `run_aborted`. Fails only if the trigger cannot continue the
    /// history, which the activation rules out before delivering it.
    pub(crate) fn abort(
        &mut self,
        at: Timestamp,
        trigger: Event,
        reason: AbortReason,
    ) -> Result<(), HistoryError> {
        tracing::warn!(%reason, "run aborted");
        for event in [trigger, Event::Lifecycle(Lifecycle::RunAborted(reason))] {
            let entry = HistoryEntry {
                index: self.state.history_len(),
                recorded_at: at,
                event,
            };
            self.state.apply(&entry)?;
            self.entries.push(entry);
        }
        Ok(())
    }

    fn exceeded_limit(&self, response: &[Event]) -> Option<AbortReason> {
        let limits = self.limits;
        let limit_exceeded = |message: String| {
            Some(AbortReason {
                kind: AbortKind::LimitExceeded,
                message,
            })
        };
        let commands = response
            .iter()
            .filter(|event| matches!(event, Event::Command(_)))
            .count();
        if !self.copying && self.commands + commands > limits.max_commands_per_activation {
            return limit_exceeded(format!(
                "the script emitted more than {} commands in one activation",
                limits.max_commands_per_activation
            ));
        }
        let length = self.state.history_len() + 1 + response.len() as u64;
        if length > limits.max_history_len {
            return limit_exceeded(format!(
                "the history would exceed {} entries",
                limits.max_history_len
            ));
        }
        for event in response {
            if let Some(size) = payloads::payload_of(event).and_then(payloads::value_size)
                && size > limits.max_payload_bytes
            {
                return limit_exceeded(format!(
                    "the payload of {} is {size} bytes, above the limit of {} bytes",
                    event.name(),
                    limits.max_payload_bytes
                ));
            }
        }
        None
    }

    /// The activation to commit, and the run's state after it.
    pub(crate) fn into_parts(self) -> (Activation, RunState) {
        let status = self.state.status();
        (
            Activation::new(self.entries, self.consumed, status),
            self.state,
        )
    }
}

/// Tool tasks, timers and cancelled timers, from the new commands (outbox pattern): a call or a
/// timer of this activation that is still pending at its end, and the cancellation of a timer of
/// an earlier one. A terminal run has no effects: its commit clears them. A task takes its route
/// and policy from the pinned toolset; a tool the toolset does not have is returned as the
/// error.
fn derive_effects(
    activation: &mut Activation,
    state: &RunState,
    toolset: &Toolset,
) -> Result<(), String> {
    if activation.status.is_terminal() {
        return Ok(());
    }
    let mut started_here = BTreeSet::new();
    for entry in &activation.entries {
        match &entry.event {
            Event::Command(Command::ScheduleToolCall {
                seq, tool, args, ..
            }) if state.pending_tool_calls().contains_key(seq) => {
                let pinned = toolset.tool(tool).ok_or_else(|| tool.clone())?;
                let server_concurrency = match &pinned.route {
                    ToolRoute::Mcp { server } => toolset.server(server).map(|s| s.concurrency),
                    _ => None,
                };
                activation.tool_calls.push(NewToolTask {
                    seq: *seq,
                    tool: tool.clone(),
                    args: args.clone(),
                    route: pinned.route.clone(),
                    policy: pinned.policy,
                    server_concurrency,
                });
            }
            Event::Command(Command::StartTimer { seq, fire_at, .. }) => {
                started_here.insert(*seq);
                if state.pending_timers().contains_key(seq) {
                    activation.timers.push(NewTimer {
                        seq: *seq,
                        fire_at: *fire_at,
                    });
                }
            }
            Event::Command(Command::CancelTimer { seq }) if !started_here.contains(seq) => {
                activation.cancelled_timers.push(*seq);
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use ptc_core::{RunMode, RunStarted, Script, Seed, Toolset};
    use serde_json::json;

    use super::*;
    use crate::adapters::{MemoryJournal, VirtualClock};
    use crate::ports::{CompiledScript, NewRun, RunMetadata, ToolDeclarations, VirtualTime};

    #[tokio::test]
    async fn the_heartbeat_renews_the_lease_and_reports_its_loss() {
        let journal = MemoryJournal::new();
        let clock = VirtualClock::new(Timestamp(1_000_000));
        let script = Script::new("s");
        let toolset = Toolset::empty();
        let tenant = crate::ports::TenantId::local();
        let compiled = CompiledScript {
            javascript: String::new(),
            source_map: String::new(),
        };
        journal.put_script(&tenant, &script, &compiled).await.unwrap();
        journal
            .put_toolset(&tenant, &toolset, &ToolDeclarations::default())
            .await
            .unwrap();
        let run = RunRef::new(tenant, ptc_core::RunId::from_u128(1));
        let started = RunStarted {
            script: script.digest(),
            toolset: toolset.digest(),
            input: json!({}).into(),
            seed: Seed::from_bytes([0; 32]),
            epoch: ptc_core::Epoch(1),
            expires_at: Timestamp(u64::MAX / 2),
            mode: RunMode::Live,
        };
        journal
            .create_run(
                NewRun {
                    run: run.clone(),
                    started,
                    metadata: RunMetadata::default(),
                    created_at: clock.now(),
                },
                None,
            )
            .await
            .unwrap();
        let lease_duration = Duration::from_secs(30);
        let lease = journal
            .acquire_run(&run, "w1", clock.now(), lease_duration)
            .await
            .unwrap()
            .unwrap();
        let mut heartbeat = LeaseHeartbeat {
            journal: &journal,
            clock: &clock,
            lease: &lease,
            every: Duration::from_secs(10),
            duration: lease_duration,
            last: clock.now(),
        };
        // Every 20 s for a minute: without renewals the lease would have expired twice.
        for _ in 0..3 {
            clock.advance_to(clock.now().saturating_add(Duration::from_secs(20)));
            heartbeat.beat().await.unwrap();
            let taken = journal
                .acquire_run(&run, "w2", clock.now(), lease_duration)
                .await
                .unwrap();
            assert_eq!(taken, None, "the renewed lease is still held");
        }
        // The worker stalls past its lease: another one takes the run, and the next beat
        // reports the loss.
        clock.advance_to(clock.now().saturating_add(Duration::from_secs(31)));
        let other = journal
            .acquire_run(&run, "w2", clock.now(), lease_duration)
            .await
            .unwrap();
        assert!(other.is_some());
        assert!(matches!(
            heartbeat.beat().await,
            Err(StoreError::Fenced { .. })
        ));
    }
}
