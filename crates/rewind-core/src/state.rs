//! The state of a run: a pure function of its history (`archi/02-history.md`, "Run state").

use std::collections::BTreeMap;
use std::fmt;

use crate::event::{
    AbortReason, Annotation, Command, Completion, Event, HistoryEntry, Lifecycle, RunMode,
    ScriptError,
};
use crate::history::{HistoryError, Validator};
use crate::ids::{Epoch, Seq};
use crate::payload::Payload;
use crate::time::Timestamp;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RunStatus {
    /// Only `run_started` is enqueued: the run has not been activated yet.
    Pending,
    Active,
    Completed,
    Failed,
    /// A `fail_run` whose error is `CancelledError`.
    Cancelled,
    /// The host ended the run (`run_aborted`).
    Aborted,
}

impl RunStatus {
    pub fn is_terminal(self) -> bool {
        !matches!(self, RunStatus::Pending | RunStatus::Active)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            RunStatus::Pending => "pending",
            RunStatus::Active => "active",
            RunStatus::Completed => "completed",
            RunStatus::Failed => "failed",
            RunStatus::Cancelled => "cancelled",
            RunStatus::Aborted => "aborted",
        }
    }
}

impl fmt::Display for RunStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a terminal run ended.
#[derive(Clone, Debug, PartialEq)]
pub enum RunOutcome {
    Completed {
        output: Payload,
    },
    /// Includes cancellations: a failure with `CancelledError`.
    Failed {
        error: ScriptError,
    },
    Aborted {
        reason: AbortReason,
    },
}

/// A wait on a signal whose mailbox was empty (`signal_awaited`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AwaitedSignal {
    pub name: String,
    /// The timer implementing the timeout, if any.
    pub timeout: Option<Seq>,
    /// Index of the `signal_awaited` entry.
    pub since: u64,
}

/// The state of a run, folded from its history.
///
/// A terminal status clears everything pending. The store keeps a copy of the status for
/// queries, but the history is authoritative.
#[derive(Clone, Debug, PartialEq)]
pub struct RunState {
    status: RunStatus,
    epoch: Option<Epoch>,
    expires_at: Option<Timestamp>,
    mode: Option<RunMode>,
    pending_calls: BTreeMap<Seq, String>,
    pending_timers: BTreeMap<Seq, Timestamp>,
    /// The index of the entry that opened each pending call and timer.
    opened: BTreeMap<Seq, u64>,
    awaited_signals: Vec<AwaitedSignal>,
    outcome: Option<RunOutcome>,
    started_at: Option<Timestamp>,
    updated_at: Option<Timestamp>,
    history_len: u64,
    validator: Validator,
}

impl Default for RunState {
    fn default() -> Self {
        Self::new()
    }
}

impl RunState {
    /// The state of a run whose history is empty: `pending`.
    pub fn new() -> Self {
        Self {
            status: RunStatus::Pending,
            epoch: None,
            expires_at: None,
            mode: None,
            pending_calls: BTreeMap::new(),
            pending_timers: BTreeMap::new(),
            opened: BTreeMap::new(),
            awaited_signals: Vec::new(),
            outcome: None,
            started_at: None,
            updated_at: None,
            history_len: 0,
            validator: Validator::default(),
        }
    }

    pub fn from_history(history: &[HistoryEntry]) -> Result<Self, HistoryError> {
        let mut state = Self::new();
        for entry in history {
            state.apply(entry)?;
        }
        Ok(state)
    }

    /// Folds the next entry of the history. The entry is checked against the history so far
    /// (see [`validate_history`](crate::validate_history)); on error the state is unchanged.
    pub fn apply(&mut self, entry: &HistoryEntry) -> Result<(), HistoryError> {
        self.validator.check(entry)?;
        self.history_len += 1;
        self.started_at.get_or_insert(entry.recorded_at);
        self.updated_at = Some(entry.recorded_at);

        match &entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(started)) => {
                self.status = RunStatus::Active;
                self.epoch = Some(started.epoch);
                self.expires_at = Some(started.expires_at);
                self.mode = Some(started.mode);
            }
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => self.end(
                RunStatus::Aborted,
                RunOutcome::Aborted {
                    reason: reason.clone(),
                },
            ),
            Event::Command(command) => self.apply_command(entry.index, command),
            Event::Completion(completion) => self.apply_completion(completion),
            Event::Annotation(Annotation::SignalAwaited { name, timeout, .. }) => {
                self.awaited_signals.push(AwaitedSignal {
                    name: name.clone(),
                    timeout: *timeout,
                    since: entry.index,
                });
            }
            Event::Annotation(Annotation::Log { .. }) => {}
        }
        Ok(())
    }

    fn apply_command(&mut self, index: u64, command: &Command) {
        match command {
            Command::ScheduleCall { seq, action, .. } => {
                self.pending_calls.insert(*seq, action.clone());
                self.opened.insert(*seq, index);
            }
            Command::StartTimer { seq, fire_at, .. } => {
                self.pending_timers.insert(*seq, *fire_at);
                self.opened.insert(*seq, index);
            }
            Command::CancelTimer { seq } => {
                if self.pending_timers.remove(seq).is_some() {
                    self.opened.remove(seq);
                }
            }
            Command::CompleteRun { output } => self.end(
                RunStatus::Completed,
                RunOutcome::Completed {
                    output: output.clone(),
                },
            ),
            Command::FailRun { error } => {
                let status = if error.name == ScriptError::CANCELLED {
                    RunStatus::Cancelled
                } else {
                    RunStatus::Failed
                };
                self.end(
                    status,
                    RunOutcome::Failed {
                        error: error.clone(),
                    },
                );
            }
        }
    }

    fn apply_completion(&mut self, completion: &Completion) {
        match completion {
            Completion::CallCompleted { seq, .. } | Completion::CallFailed { seq, .. } => {
                if self.pending_calls.remove(seq).is_some() {
                    self.opened.remove(seq);
                }
            }
            Completion::TimerFired { seq } => {
                if self.pending_timers.remove(seq).is_some() {
                    self.opened.remove(seq);
                }
                self.awaited_signals
                    .retain(|wait| wait.timeout != Some(*seq));
            }
            Completion::SignalReceived { name, .. } => {
                if let Some(position) = self
                    .awaited_signals
                    .iter()
                    .position(|wait| &wait.name == name)
                {
                    self.awaited_signals.remove(position);
                }
            }
            // Every pending await is rejected, signal waits included.
            Completion::CancelRequested { .. } => self.awaited_signals.clear(),
        }
    }

    fn end(&mut self, status: RunStatus, outcome: RunOutcome) {
        self.status = status;
        self.outcome = Some(outcome);
        self.pending_calls.clear();
        self.pending_timers.clear();
        self.opened.clear();
        self.awaited_signals.clear();
    }

    pub fn status(&self) -> RunStatus {
        self.status
    }

    pub fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    /// The epoch recorded in `run_started`.
    pub fn epoch(&self) -> Option<Epoch> {
        self.epoch
    }

    /// The deadline recorded in `run_started`.
    pub fn expires_at(&self) -> Option<Timestamp> {
        self.expires_at
    }

    pub fn mode(&self) -> Option<RunMode> {
        self.mode
    }

    /// Calls scheduled and still without an outcome, by `seq`.
    pub fn pending_calls(&self) -> &BTreeMap<Seq, String> {
        &self.pending_calls
    }

    /// Timers started and neither fired nor cancelled, by `seq`, with their `fire_at`.
    pub fn pending_timers(&self) -> &BTreeMap<Seq, Timestamp> {
        &self.pending_timers
    }

    /// The index of the entry that opened the pending call or timer `seq`
    /// (`schedule_call`, `start_timer`): what is derived from a pending call or timer
    /// reads its opening entry without walking the history.
    pub fn opened_by(&self, seq: Seq) -> Option<u64> {
        self.opened.get(&seq).copied()
    }

    pub fn awaited_signals(&self) -> &[AwaitedSignal] {
        &self.awaited_signals
    }

    /// How the run ended, once it has.
    pub fn outcome(&self) -> Option<&RunOutcome> {
        self.outcome.as_ref()
    }

    pub fn started_at(&self) -> Option<Timestamp> {
        self.started_at
    }

    /// `recorded_at` of the last entry.
    pub fn updated_at(&self) -> Option<Timestamp> {
        self.updated_at
    }

    /// Number of entries folded, which is also the index of the next entry.
    pub fn history_len(&self) -> u64 {
        self.history_len
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::digest::Digest;
    use crate::event::{AbortKind, CancelReason, RunStarted};
    use crate::ids::Seed;

    fn entries(events: Vec<Event>) -> Vec<HistoryEntry> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, event)| HistoryEntry {
                index: i as u64,
                recorded_at: Timestamp(1_000 + i as u64),
                event,
            })
            .collect()
    }

    fn started() -> Event {
        Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            code: Digest::of(b"script"),
            action_set: Digest::of(b"action_set"),
            input: json!({}).into(),
            seed: Seed::from_bytes([7; 32]),
            epoch: Epoch(1),
            expires_at: Timestamp(9_999),
            mode: RunMode::Mock,
        }))
    }

    fn call(seq: u32) -> Event {
        Event::Command(Command::ScheduleCall {
            seq: Seq(seq),
            action: format!("action_{seq}"),
            args: json!({}).into(),
            callsite: None,
        })
    }

    fn timer(seq: u32, fire_at: u64) -> Event {
        Event::Command(Command::StartTimer {
            seq: Seq(seq),
            duration_ms: fire_at,
            fire_at: Timestamp(fire_at),
            callsite: None,
        })
    }

    fn awaited(name: &str, timeout: Option<u32>) -> Event {
        Event::Annotation(Annotation::SignalAwaited {
            name: name.into(),
            timeout: timeout.map(Seq),
            callsite: None,
        })
    }

    fn fail(name: &str) -> Event {
        Event::Command(Command::FailRun {
            error: ScriptError {
                name: name.into(),
                message: "x".into(),
                stack: None,
            },
        })
    }

    #[test]
    fn an_empty_history_is_pending() {
        let state = RunState::from_history(&[]).unwrap();
        assert_eq!(state.status(), RunStatus::Pending);
        assert_eq!(state.history_len(), 0);
        assert_eq!(state.started_at(), None);
    }

    #[test]
    fn tracks_what_is_pending() {
        let history = entries(vec![
            started(),
            call(1),
            call(2),
            timer(3, 5_000),
            Event::Completion(Completion::CallCompleted {
                seq: Seq(2),
                result: json!(1).into(),
            }),
            timer(4, 6_000),
            awaited("approval", Some(4)),
            awaited("comment", None),
        ]);
        let state = RunState::from_history(&history).unwrap();
        assert_eq!(state.status(), RunStatus::Active);
        assert_eq!(state.epoch(), Some(Epoch(1)));
        assert_eq!(state.expires_at(), Some(Timestamp(9_999)));
        assert_eq!(
            state.pending_calls().keys().copied().collect::<Vec<_>>(),
            [Seq(1)]
        );
        assert_eq!(
            state.pending_timers().keys().copied().collect::<Vec<_>>(),
            [Seq(3), Seq(4)]
        );
        assert_eq!(state.awaited_signals().len(), 2);
        assert_eq!(state.started_at(), Some(Timestamp(1_000)));
        assert_eq!(state.updated_at(), Some(Timestamp(1_007)));

        // The timeout of "approval" fires; a "comment" arrives.
        let mut state = state;
        state
            .apply(&HistoryEntry {
                index: 8,
                recorded_at: Timestamp(6_000),
                event: Event::Completion(Completion::TimerFired { seq: Seq(4) }),
            })
            .unwrap();
        state
            .apply(&HistoryEntry {
                index: 9,
                recorded_at: Timestamp(6_001),
                event: Event::Completion(Completion::SignalReceived {
                    name: "comment".into(),
                    payload: json!("lgtm").into(),
                }),
            })
            .unwrap();
        assert!(state.awaited_signals().is_empty());
        assert_eq!(
            state.pending_timers().keys().copied().collect::<Vec<_>>(),
            [Seq(3)]
        );
    }

    #[test]
    fn a_terminal_status_clears_everything_pending() {
        let history = entries(vec![
            started(),
            call(1),
            timer(2, 5_000),
            awaited("approval", None),
            Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
                kind: AbortKind::LimitExceeded,
                message: "cpu".into(),
            })),
        ]);
        let state = RunState::from_history(&history).unwrap();
        assert_eq!(state.status(), RunStatus::Aborted);
        assert!(state.is_terminal());
        assert!(state.pending_calls().is_empty());
        assert!(state.pending_timers().is_empty());
        assert!(state.awaited_signals().is_empty());
        assert!(matches!(state.outcome(), Some(RunOutcome::Aborted { .. })));
    }

    #[test]
    fn a_cancelled_error_means_cancelled() {
        let cancelled = entries(vec![
            started(),
            Event::Completion(Completion::CancelRequested {
                reason: CancelReason::Ttl,
            }),
            fail(ScriptError::CANCELLED),
        ]);
        assert_eq!(
            RunState::from_history(&cancelled).unwrap().status(),
            RunStatus::Cancelled
        );
        let failed = entries(vec![started(), fail("TypeError")]);
        assert_eq!(
            RunState::from_history(&failed).unwrap().status(),
            RunStatus::Failed
        );
    }

    #[test]
    fn rejects_malformed_histories() {
        let err = |events: Vec<Event>| RunState::from_history(&entries(events)).unwrap_err();
        assert_eq!(err(vec![call(1)]), HistoryError::MissingRunStarted);
        assert_eq!(
            err(vec![started(), started()]),
            HistoryError::DuplicateRunStarted { index: 1 }
        );
        assert_eq!(
            err(vec![started(), fail("Error"), call(1)]),
            HistoryError::EntriesAfterEnd { index: 2 }
        );

        let mut gap = entries(vec![started(), call(1)]);
        gap[1].index = 5;
        assert_eq!(
            RunState::from_history(&gap).unwrap_err(),
            HistoryError::NonContiguous {
                position: 1,
                index: 5
            }
        );

        let mut backwards = entries(vec![started(), call(1)]);
        backwards[1].recorded_at = Timestamp(10);
        assert_eq!(
            RunState::from_history(&backwards).unwrap_err(),
            HistoryError::TimeWentBackwards { index: 1 }
        );
    }

    #[test]
    fn a_rejected_entry_leaves_the_state_unchanged() {
        let mut state = RunState::from_history(&entries(vec![started()])).unwrap();
        let before = state.clone();
        let wrong_index = HistoryEntry {
            index: 3,
            recorded_at: Timestamp(2_000),
            event: call(1),
        };
        assert!(state.apply(&wrong_index).is_err());
        assert_eq!(state, before);
    }
}
