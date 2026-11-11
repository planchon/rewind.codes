//! Structural validation of a history (`archi/03-execution.md`, "Replay").

use crate::event::{Event, HistoryEntry, Lifecycle, RunStarted};
use crate::time::Timestamp;

/// A history that cannot have been written by the engine.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    #[error("the history is empty")]
    Empty,
    #[error("the history does not start with run_started")]
    MissingRunStarted,
    #[error("the entry at position {position} has index {index}")]
    NonContiguous { position: u64, index: u64 },
    #[error("entry {index} is a second run_started")]
    DuplicateRunStarted { index: u64 },
    #[error("entry {index} follows the end of the run")]
    EntriesAfterEnd { index: u64 },
    #[error("entry {index} is recorded before the entry that precedes it")]
    TimeWentBackwards { index: u64 },
}

/// Checks that `history` is well formed and returns its `run_started`.
///
/// A well-formed history is not empty, starts with exactly one `run_started`, has contiguous
/// indexes from 0 and monotonic `recorded_at`, and has nothing after the entry that ended the
/// run (`complete_run`, `fail_run` or `run_aborted`).
pub fn validate_history(history: &[HistoryEntry]) -> Result<&RunStarted, HistoryError> {
    let first = history.first().ok_or(HistoryError::Empty)?;
    let mut validator = Validator::default();
    for entry in history {
        validator.check(entry)?;
    }
    match &first.event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => Ok(started),
        _ => Err(HistoryError::MissingRunStarted),
    }
}

/// Incremental form of [`validate_history`], entry by entry.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Validator {
    next_index: u64,
    last_recorded_at: Option<Timestamp>,
    ended: bool,
}

impl Validator {
    pub(crate) fn check(&mut self, entry: &HistoryEntry) -> Result<(), HistoryError> {
        let position = self.next_index;
        if entry.index != position {
            return Err(HistoryError::NonContiguous {
                position,
                index: entry.index,
            });
        }
        let is_run_started = matches!(entry.event, Event::Lifecycle(Lifecycle::RunStarted(_)));
        if position == 0 && !is_run_started {
            return Err(HistoryError::MissingRunStarted);
        }
        if position > 0 && is_run_started {
            return Err(HistoryError::DuplicateRunStarted { index: entry.index });
        }
        if self.ended {
            return Err(HistoryError::EntriesAfterEnd { index: entry.index });
        }
        if self
            .last_recorded_at
            .is_some_and(|last| entry.recorded_at < last)
        {
            return Err(HistoryError::TimeWentBackwards { index: entry.index });
        }

        self.next_index += 1;
        self.last_recorded_at = Some(entry.recorded_at);
        self.ended = entry.event.is_terminal();
        Ok(())
    }
}
