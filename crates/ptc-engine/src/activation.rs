//! One activation (`archi/03-execution.md`, "Activations").
//!
//! ```text
//! history ← load_history(run); instance ← start + replay(history)
//! for trigger in take_inbox(lease), in arrival order:
//!     recorded_at ← max(now, last recorded_at)            # monotonic per run
//!     if the run is terminal: drop the trigger
//!     append the trigger; deliver it; append what the script emitted
//! offload large payloads; derive tool tasks and timers from the new commands
//! commit_activation(lease, …)                             # one transaction, fenced
//! ```
//!
//! A trigger is always committed with everything the script emitted in response to it (the
//! commit invariant). When the host must end the run instead (the script cannot load, a limit
//! is exceeded, the cancellation grace runs out), the trigger is committed with `run_aborted`
//! and what the script emitted in response to it is dropped. A replay mismatch commits
//! nothing: the run is quarantined.

use std::cmp::max;

use ptc_core::{
    AbortKind, AbortReason, Command, Completion, Event, HistoryEntry, HistoryError, Lifecycle,
    RunState, Timestamp,
};
use tracing::Instrument;

use crate::engine::{ActivationOutcome, Engine, EngineError, Limits};
use crate::payloads;
use crate::ports::{
    Activation, BlobStore, Clock, Emitted, Entropy, ExecutorError, JournalStore, Lease, NewTimer,
    NewToolTask, ScriptExecutor, ScriptInstance, StartScript, ToolDispatcher, Trigger,
};
use crate::replay::{self, Rebuilt, ReplayEnd, ReplayError, ReplayInput};

impl<J, B, X, D, C, E> Engine<J, B, X, D, C, E>
where
    J: JournalStore,
    B: BlobStore,
    X: ScriptExecutor,
    D: ToolDispatcher,
    C: Clock,
    E: Entropy,
{
    /// Activates the leased run: replays its history, delivers its inbox, commits. The lease
    /// is released on success; on error the caller releases it.
    pub(crate) async fn activate(&self, lease: &Lease) -> Result<ActivationOutcome, EngineError> {
        let span = tracing::info_span!("activation", run_id = %lease.run_id, fence = lease.fence);
        self.activate_in_span(lease).instrument(span).await
    }

    async fn activate_in_span(&self, lease: &Lease) -> Result<ActivationOutcome, EngineError> {
        let run_id = lease.run_id;
        let inbox = self.journal.take_inbox(lease).await?;
        if inbox.is_empty() {
            self.journal.release(lease).await?;
            return Ok(ActivationOutcome::NothingToDo);
        }

        let record = self.run_record(run_id).await?;
        let stored = self.stored_script(record.script).await?;
        let toolset = self.toolset(record.toolset).await?;
        let history = self.journal.load_history(run_id, 0).await?;
        let state = RunState::from_history(&history)
            .map_err(|error| EngineError::CorruptHistory { run_id, error })?;

        // Full replay on every activation: instances are not kept between activations.
        let mut instance = None;
        if !history.is_empty() && !state.is_terminal() {
            let input = ReplayInput {
                run_id,
                compiled: Ok(&stored.compiled),
                toolset: &toolset,
                history: &history,
            };
            match replay::replay(&self.executor, &self.blobs, input).await {
                Ok(Rebuilt {
                    instance: Some(rebuilt),
                    end: ReplayEnd::EndOfHistory,
                }) => instance = Some(rebuilt),
                Ok(_) => {
                    return Err(EngineError::CorruptInbox {
                        run_id,
                        message: "the replay of a run that has not ended stopped early".into(),
                    });
                }
                // A script cannot be non-deterministic by construction: a mismatch is a fault
                // of the platform, never of the script. The run waits for a fix.
                Err(ReplayError::NonDeterminism(mismatch)) => {
                    return self.quarantine(lease, mismatch.to_string()).await;
                }
                // A script that loaded before and no longer does, or an epoch no longer
                // supported, is a fault of the platform: the run waits for a fix.
                Err(ReplayError::Executor(
                    err @ (ExecutorError::InvalidScript(_) | ExecutorError::UnsupportedEpoch(_)),
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
            }
        }

        let last_recorded_at = state.updated_at().unwrap_or(Timestamp(0));
        let mut batch = Batch::new(state, last_recorded_at, &self.config.limits);
        for item in inbox {
            batch.consumed.push(item.id);
            if batch.state.is_terminal() {
                // A completion for a run that has ended: dropped.
                continue;
            }
            let recorded_at = batch.recorded_at(self.clock.now());
            match item.trigger {
                Trigger::RunStarted(started) => {
                    if batch.state.history_len() != 0 {
                        return Err(EngineError::CorruptInbox {
                            run_id,
                            message: "run_started for a run that has already started".into(),
                        });
                    }
                    let input = payloads::load(&self.blobs, &started.input).await?;
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
                    let delivery = payloads::delivery(&self.blobs, &completion).await?;
                    let grace = match completion {
                        Completion::CancelRequested { .. } => Grace::Ends,
                        _ => Grace::No,
                    };
                    let trigger = Event::Completion(completion);
                    match running.deliver(delivery, recorded_at).await {
                        Ok(emitted) => batch.respond(recorded_at, trigger, emitted, grace),
                        Err(err) => batch.abort(recorded_at, trigger, abort_reason(err)),
                    }
                    .map_err(|error| EngineError::CorruptHistory { run_id, error })?;
                }
            }
        }

        let status = batch.state.status();
        let mut activation = batch.into_activation();
        payloads::offload(
            &self.blobs,
            &mut activation.entries,
            self.config.inline_threshold,
        )
        .await?;
        derive_effects(&mut activation);
        let entries = activation.entries.len();
        self.journal.commit_activation(lease, activation).await?;
        self.journal.release(lease).await?;
        tracing::debug!(entries, status = %status, "activation committed");
        Ok(ActivationOutcome::Committed { entries, status })
    }

    async fn quarantine(
        &self,
        lease: &Lease,
        mismatch: String,
    ) -> Result<ActivationOutcome, EngineError> {
        tracing::error!(%mismatch, "replay mismatch: run quarantined");
        self.journal.quarantine(lease, mismatch.clone()).await?;
        Ok(ActivationOutcome::Quarantined { mismatch })
    }
}

/// Whether delivering a trigger ends the cancellation grace: after `cancel_requested`, the
/// script gets this one delivery to end by itself.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Grace {
    No,
    Ends,
}

fn abort_reason(err: ExecutorError) -> AbortReason {
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
struct Batch<'l> {
    state: RunState,
    last_recorded_at: Timestamp,
    entries: Vec<HistoryEntry>,
    consumed: Vec<u64>,
    commands: usize,
    limits: &'l Limits,
}

impl<'l> Batch<'l> {
    fn new(state: RunState, last_recorded_at: Timestamp, limits: &'l Limits) -> Self {
        Self {
            state,
            last_recorded_at,
            entries: Vec::new(),
            consumed: Vec::new(),
            commands: 0,
            limits,
        }
    }

    /// `max(now, last recorded_at)`: monotonic per run.
    fn recorded_at(&mut self, now: Timestamp) -> Timestamp {
        self.last_recorded_at = max(now, self.last_recorded_at);
        self.last_recorded_at
    }

    /// Appends a trigger and what the script emitted in response, or the trigger and
    /// `run_aborted` if the response breaks a limit or the cancellation grace.
    fn respond(
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

        self.commands += group
            .iter()
            .filter(|entry| matches!(entry.event, Event::Command(_)))
            .count();
        self.state = state;
        self.entries.extend(group);
        Ok(())
    }

    /// Appends the trigger and `run_aborted`. Fails only if the trigger cannot continue the
    /// history, which the activation rules out before delivering it.
    fn abort(
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
        if self.commands + commands > limits.max_commands_per_activation {
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

    fn into_activation(self) -> Activation {
        Activation {
            entries: self.entries,
            consumed: self.consumed,
            tool_calls: Vec::new(),
            timers: Vec::new(),
            cancelled_timers: Vec::new(),
            status: self.state.status(),
        }
    }
}

/// Tool tasks, timers and cancelled timers, from the new commands (outbox pattern). A
/// terminal run has no effects: its commit clears them.
fn derive_effects(activation: &mut Activation) {
    if activation.status.is_terminal() {
        return;
    }
    for entry in &activation.entries {
        match &entry.event {
            Event::Command(Command::ScheduleToolCall {
                seq, tool, args, ..
            }) => activation.tool_calls.push(NewToolTask {
                seq: *seq,
                tool: tool.clone(),
                args: args.clone(),
            }),
            Event::Command(Command::StartTimer { seq, fire_at, .. }) => {
                activation.timers.push(NewTimer {
                    seq: *seq,
                    fire_at: *fire_at,
                });
            }
            Event::Command(Command::CancelTimer { seq }) => {
                match activation.timers.iter().position(|timer| timer.seq == *seq) {
                    Some(started_here) => {
                        activation.timers.remove(started_here);
                    }
                    None => activation.cancelled_timers.push(*seq),
                }
            }
            _ => {}
        }
    }
}
