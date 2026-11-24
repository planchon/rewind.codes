//! The entries of one activation, as they are appended (`archi/02-journal.md`, "The commit
//! invariant"; `archi/03-execution.md`, "Activations", "How a code ends").
//!
//! A trigger is always appended with everything the code emitted in response to it. When the
//! host must end the run instead (the code cannot load, a limit is exceeded, the cancellation
//! grace runs out), the trigger is appended with `run_aborted` and what the code emitted in
//! response to it is dropped. The run's state is folded as the entries are appended, so a
//! terminal entry ends the batch. An activation that fails otherwise appends nothing: its
//! [`EngineError`] reaches the owner, which records the failure.

use std::cmp::max;
use std::time::Duration;

use rewind_core::{
    AbortKind, AbortReason, Event, HistoryEntry, HistoryError, Lifecycle, RunId, RunState,
    Timestamp,
};

use crate::payloads;
use crate::plane::Limits;
use crate::ports::{BlobError, Emitted, ExecutorError, RunRef, SecretError, StoreError};
use crate::text::NulCharacter;

/// Why an activation, a replay or a fork's first activation could not go on: what it appended
/// is dropped, and its owner records the failure.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Blob(#[from] BlobError),
    /// A command names an action its run's action set does not have.
    #[error("run {run_id} called {action}, which its action set does not have")]
    UnknownAction { run_id: RunId, action: String },
    #[error(transparent)]
    Secret(#[from] SecretError),
    /// A text to store holds a NUL character.
    #[error(transparent)]
    NulCharacter(#[from] NulCharacter),
    #[error("the history of run {run_id} is invalid: {error}")]
    CorruptHistory { run_id: RunId, error: HistoryError },
    #[error("the inbox of run {run_id} is invalid: {message}")]
    CorruptInbox { run_id: RunId, message: String },
    /// The executor could not rebuild the run, for a reason that may be transient (a limit
    /// hit under load, an internal error): the activation is retried later.
    #[error("the replay of run {run_id} could not go on: {error}")]
    ReplayInterrupted { run_id: RunId, error: ExecutorError },
}

/// Whether delivering a trigger ends the cancellation grace: after `cancel_requested`, the
/// code gets this one delivery to end by itself.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Grace {
    No,
    Ends,
}

pub(crate) fn abort_reason(err: ExecutorError) -> AbortReason {
    let kind = match &err {
        ExecutorError::InvalidCode(_) => AbortKind::InvalidCode,
        ExecutorError::LimitExceeded(_) => AbortKind::LimitExceeded,
        ExecutorError::UnsupportedEpoch(_) | ExecutorError::Internal(_) => AbortKind::Internal,
    };
    let message = match err {
        ExecutorError::InvalidCode(message) | ExecutorError::LimitExceeded(message) => message,
        other => other.to_string(),
    };
    AbortReason { kind, message }
}

/// Where the CPU time of the calls into a code that execute is counted, toward a namespace's
/// `cpuPerMonth` (`archi/06-storage-and-scale.md`, "Quotas").
pub(crate) trait CountCpu {
    fn count_cpu(
        &self,
        tenant: &crate::ports::TenantId,
        namespace: &crate::ports::NamespaceName,
        cpu: Duration,
    );
}

/// What a batch appended: its entries, the inbox items it consumed, the run's state before the
/// entries and after them.
pub(crate) struct Finished {
    pub(crate) entries: Vec<HistoryEntry>,
    pub(crate) consumed: Vec<u64>,
    pub(crate) before: RunState,
    pub(crate) after: RunState,
}

/// The entries of one activation, folded into the run's state as they are appended.
pub(crate) struct Batch<'l> {
    pub(crate) run_id: rewind_core::RunId,
    pub(crate) tenant: crate::ports::TenantId,
    /// The namespace whose `cpu_per_month` the calls into the code count toward; `None` when
    /// they run nothing of a run's, as a fork's compatibility check.
    namespace: Option<crate::ports::NamespaceName>,
    pub(crate) state: RunState,
    /// The state before the batch's first entry: what a refused response is undone from.
    initial: RunState,
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
    pub(crate) fn new(
        run: &RunRef,
        namespace: Option<crate::ports::NamespaceName>,
        state: RunState,
        last_recorded_at: Timestamp,
        limits: &'l Limits,
    ) -> Self {
        Self {
            run_id: run.id,
            tenant: run.tenant.clone(),
            namespace,
            initial: state.clone(),
            state,
            last_recorded_at,
            entries: Vec::new(),
            consumed: Vec::new(),
            commands: 0,
            copying: false,
            limits,
        }
    }

    /// An empty batch for a run whose history is empty, whose calls into the code count
    /// toward no namespace: a fork's compatibility check.
    pub(crate) fn for_check(run: &RunRef, limits: &'l Limits) -> Self {
        Self::new(run, None, RunState::new(), Timestamp(0), limits)
    }

    /// Counts the CPU time of a call into the code that executed, toward the namespace of
    /// the run: never a call of a replay (`archi/06-storage-and-scale.md`, "Quotas").
    pub(crate) fn count_cpu(&self, sink: &impl CountCpu, cpu: Duration) {
        if let Some(namespace) = &self.namespace {
            sink.count_cpu(&self.tenant, namespace, cpu);
        }
    }

    /// The entries appended, the inbox items consumed, and the run's state before and after
    /// them.
    pub(crate) fn finish(self) -> Finished {
        Finished {
            entries: self.entries,
            consumed: self.consumed,
            before: self.initial,
            after: self.state,
        }
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

    /// Appends a trigger and what the code emitted in response, or the trigger and
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

        // Folded in place: a copy of the state for each response would cost the size of the
        // state (every pending timer and call) at each delivery. A response refused is undone.
        let start = self.entries.len();
        let mut refused = None;
        for event in std::iter::once(trigger.clone()).chain(response) {
            let entry = HistoryEntry {
                index: self.state.history_len(),
                recorded_at: at,
                event,
            };
            if let Err(err) = self.state.apply(&entry) {
                refused = Some(AbortReason {
                    kind: AbortKind::Internal,
                    message: format!("the executor emitted an invalid response: {err}"),
                });
                break;
            }
            self.entries.push(entry);
        }
        if refused.is_none() && grace == Grace::Ends && !self.state.is_terminal() {
            refused = Some(AbortReason {
                kind: AbortKind::Cancelled,
                message: "the code did not end within its cancellation grace".into(),
            });
        }
        if let Some(reason) = refused {
            self.undo(start)?;
            return self.abort(at, trigger, reason);
        }

        if !self.copying {
            self.commands += self.entries[start..]
                .iter()
                .filter(|entry| matches!(entry.event, Event::Command(_)))
                .count();
        }
        Ok(())
    }

    /// Drops the entries appended from `start` on, and folds the state again without them,
    /// from the state the batch began with.
    fn undo(&mut self, start: usize) -> Result<(), HistoryError> {
        self.entries.truncate(start);
        let mut state = self.initial.clone();
        for entry in &self.entries {
            state.apply(entry)?;
        }
        self.state = state;
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
                "the code emitted more than {} commands in one activation",
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
}
