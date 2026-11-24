//! The determinism check: re-executing a run against its recorded history
//! (`archi/03-execution.md`, "Replay").
//!
//! Whoever drives the script (the engine) drives a [`ReplayCursor`]:
//!
//! 1. Start the script, and report every command it emits to [`ReplayCursor::on_command`].
//! 2. When the script is idle, call [`ReplayCursor::on_idle`]:
//!    - [`Next::Deliver`]: deliver that completion, then back to step 1;
//!    - [`Next::Aborted`]: the host aborted the original run here, stop;
//!    - [`Next::End`]: the history is exhausted. The script is in exactly the state it was in
//!      after the last commit, and the run can continue live.
//!
//! This relies on the commit invariant: every entry the script emitted in response to a
//! trigger is recorded with it. So until `End`, every emitted command has a recorded
//! counterpart, and every annotation emitted is already recorded.

use crate::event::{AbortReason, Command, Completion, Event, HistoryEntry, Lifecycle, RunStarted};
use crate::history::{HistoryError, validate_history};
use crate::time::Timestamp;

#[derive(Debug)]
pub struct ReplayCursor<'h> {
    history: &'h [HistoryEntry],
    started: &'h RunStarted,
    /// Position of the next entry to consume.
    pos: usize,
    logical_now: Timestamp,
}

/// Outcome of a command the script emitted during replay.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Replayed<'h> {
    /// The command matches the history entry at `index`.
    Matched { index: u64 },
    /// The host aborted the original run at this point; stop replaying.
    Aborted { index: u64, reason: &'h AbortReason },
}

/// What happens once the script is idle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Next<'h> {
    /// Deliver this completion, recorded at the cursor's new [`ReplayCursor::logical_now`].
    Deliver {
        index: u64,
        completion: &'h Completion,
    },
    /// The host aborted the original run here.
    Aborted { index: u64, reason: &'h AbortReason },
    /// The history is exhausted.
    End,
}

/// The script did not do what the history says it did. Messages are written to be read by a
/// model.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum NonDeterminism {
    #[error("history entry {index} is {recorded}, but the script emitted {emitted}")]
    CommandMismatch {
        index: u64,
        recorded: Box<Command>,
        emitted: Box<Command>,
    },
    #[error("the script emitted {emitted}, which the history does not contain (at entry {at})")]
    UnexpectedCommand { at: u64, emitted: Box<Command> },
    #[error("history entry {index} is {expected}, but the script went idle without emitting it")]
    MissingCommand { index: u64, expected: Box<Command> },
}

impl<'h> ReplayCursor<'h> {
    /// A cursor at the start of `history`, which is validated first
    /// (see [`validate_history`]).
    pub fn new(history: &'h [HistoryEntry]) -> Result<Self, HistoryError> {
        let started = validate_history(history)?;
        Ok(Self {
            history,
            started,
            pos: 1,
            logical_now: history[0].recorded_at,
        })
    }

    pub fn run_started(&self) -> &'h RunStarted {
        self.started
    }

    /// `recorded_at` of `run_started`: the script's `run.startedAt`.
    pub fn started_at(&self) -> Timestamp {
        self.history[0].recorded_at
    }

    /// The run's logical time: `recorded_at` of the last trigger delivered.
    pub fn logical_now(&self) -> Timestamp {
        self.logical_now
    }

    /// Reports a command the script just emitted.
    pub fn on_command(&mut self, emitted: &Command) -> Result<Replayed<'h>, NonDeterminism> {
        let Some(entry) = self.peek() else {
            return Err(NonDeterminism::UnexpectedCommand {
                at: self.history.len() as u64,
                emitted: Box::new(emitted.clone()),
            });
        };
        match &entry.event {
            Event::Command(recorded) if recorded.matches(emitted) => {
                self.pos += 1;
                Ok(Replayed::Matched { index: entry.index })
            }
            Event::Command(recorded) => Err(NonDeterminism::CommandMismatch {
                index: entry.index,
                recorded: Box::new(recorded.clone()),
                emitted: Box::new(emitted.clone()),
            }),
            // The commands the original run emitted here were dropped with the abort.
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => Ok(Replayed::Aborted {
                index: entry.index,
                reason,
            }),
            Event::Completion(_)
            | Event::Lifecycle(Lifecycle::RunStarted(_))
            | Event::Annotation(_) => Err(NonDeterminism::UnexpectedCommand {
                at: entry.index,
                emitted: Box::new(emitted.clone()),
            }),
        }
    }

    /// Reports that the script is idle, and says what happens next.
    pub fn on_idle(&mut self) -> Result<Next<'h>, NonDeterminism> {
        let Some(entry) = self.peek() else {
            return Ok(Next::End);
        };
        match &entry.event {
            Event::Completion(completion) => {
                self.pos += 1;
                self.logical_now = entry.recorded_at;
                Ok(Next::Deliver {
                    index: entry.index,
                    completion,
                })
            }
            Event::Command(expected) => Err(NonDeterminism::MissingCommand {
                index: entry.index,
                expected: Box::new(expected.clone()),
            }),
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => Ok(Next::Aborted {
                index: entry.index,
                reason,
            }),
            // `validate_history` rejects a second `run_started`, and `peek` skips annotations.
            Event::Lifecycle(Lifecycle::RunStarted(_)) | Event::Annotation(_) => {
                unreachable!("not returned by peek on a validated history")
            }
        }
    }

    /// The reason the original run was aborted, if it was aborted at this point: the next
    /// entry that is not an annotation is `run_aborted`.
    ///
    /// When the script cannot go on (it failed to load, or hit a resource limit), this tells
    /// whether the original run stopped at the same point.
    pub fn aborted_here(&mut self) -> Option<&'h AbortReason> {
        match &self.peek()?.event {
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => Some(reason),
            _ => None,
        }
    }

    /// The next entry that is not an annotation.
    fn peek(&mut self) -> Option<&'h HistoryEntry> {
        while let Some(entry) = self.history.get(self.pos) {
            if !matches!(entry.event, Event::Annotation(_)) {
                return Some(entry);
            }
            self.pos += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;
    use crate::digest::Digest;
    use crate::event::{AbortKind, Annotation, Callsite, LogLevel, RunMode};
    use crate::ids::{Epoch, Seed, Seq};

    fn started() -> Event {
        Event::Lifecycle(Lifecycle::RunStarted(RunStarted {
            script: Digest::of(b"script"),
            action_set: Digest::of(b"action_set"),
            input: json!({}).into(),
            seed: Seed::from_bytes([7; 32]),
            epoch: Epoch(1),
            expires_at: Timestamp(1_000 + 30 * 86_400_000),
            mode: RunMode::Live,
        }))
    }

    fn call(seq: u32, action: &str, args: Value) -> Command {
        Command::ScheduleCall {
            seq: Seq(seq),
            action: action.into(),
            args: args.into(),
            callsite: None,
        }
    }

    fn done(seq: u32, result: Value) -> Event {
        Event::Completion(Completion::CallCompleted {
            seq: Seq(seq),
            result: result.into(),
        })
    }

    fn complete(output: Value) -> Command {
        Command::CompleteRun {
            output: output.into(),
        }
    }

    fn log(message: &str) -> Event {
        Event::Annotation(Annotation::Log {
            level: LogLevel::Info,
            message: message.into(),
            callsite: None,
        })
    }

    /// Entry `i` is recorded at `1000 + 10 * i` ms.
    fn history(events: Vec<Event>) -> Vec<HistoryEntry> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, event)| HistoryEntry {
                index: i as u64,
                recorded_at: Timestamp(1_000 + 10 * i as u64),
                event,
            })
            .collect()
    }

    fn delivered(next: Next<'_>) -> &Completion {
        match next {
            Next::Deliver { completion, .. } => completion,
            other => panic!("expected a delivery, got {other:?}"),
        }
    }

    #[test]
    fn replays_a_sequential_run_to_the_end() {
        let h = history(vec![
            started(),
            Event::Command(call(1, "github.list_issues", json!({ "repo": "acme/api" }))),
            done(1, json!([1, 2, 3])),
            Event::Command(complete(json!({ "count": 3 }))),
        ]);
        let mut cursor = ReplayCursor::new(&h).unwrap();
        assert_eq!(cursor.logical_now(), Timestamp(1_000));
        assert_eq!(cursor.started_at(), Timestamp(1_000));

        let first = call(1, "github.list_issues", json!({ "repo": "acme/api" }));
        assert_eq!(
            cursor.on_command(&first).unwrap(),
            Replayed::Matched { index: 1 }
        );

        let completion = delivered(cursor.on_idle().unwrap());
        assert_eq!(
            completion,
            &Completion::CallCompleted {
                seq: Seq(1),
                result: json!([1, 2, 3]).into()
            }
        );
        assert_eq!(cursor.logical_now(), Timestamp(1_020));

        let last = complete(json!({ "count": 3 }));
        assert_eq!(
            cursor.on_command(&last).unwrap(),
            Replayed::Matched { index: 3 }
        );
        assert_eq!(cursor.on_idle().unwrap(), Next::End);
    }

    #[test]
    fn delivers_concurrent_completions_in_recorded_order() {
        // Promise.all([a(), b()]): b answered first in the original run.
        let h = history(vec![
            started(),
            Event::Command(call(1, "a", json!({}))),
            Event::Command(call(2, "b", json!({}))),
            done(2, json!("b")),
            done(1, json!("a")),
            Event::Command(complete(json!(["a", "b"]))),
        ]);
        let mut cursor = ReplayCursor::new(&h).unwrap();
        cursor.on_command(&call(1, "a", json!({}))).unwrap();
        cursor.on_command(&call(2, "b", json!({}))).unwrap();

        let seqs: Vec<Seq> = (0..2)
            .map(|_| match delivered(cursor.on_idle().unwrap()) {
                Completion::CallCompleted { seq, .. } => *seq,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(seqs, [Seq(2), Seq(1)]);

        cursor.on_command(&complete(json!(["a", "b"]))).unwrap();
        assert_eq!(cursor.on_idle().unwrap(), Next::End);
    }

    #[test]
    fn args_held_as_digests_are_compared_by_digest() {
        let args = json!({ "number": 4 });
        let h = history(vec![
            started(),
            Event::Command(Command::ScheduleCall {
                seq: Seq(1),
                action: "github.close_issue".into(),
                args: crate::Payload::Digest(crate::Payload::digest_of(&args)),
                callsite: None,
            }),
        ]);
        let mut cursor = ReplayCursor::new(&h).unwrap();
        assert_eq!(
            cursor
                .on_command(&call(1, "github.close_issue", json!({ "number": 4 })))
                .unwrap(),
            Replayed::Matched { index: 1 }
        );
    }

    #[test]
    fn detects_changed_args() {
        let h = history(vec![
            started(),
            Event::Command(call(1, "github.close_issue", json!({ "number": 4 }))),
        ]);
        let mut cursor = ReplayCursor::new(&h).unwrap();
        let err = cursor
            .on_command(&call(1, "github.close_issue", json!({ "number": 5 })))
            .unwrap_err();
        assert!(matches!(
            err,
            NonDeterminism::CommandMismatch { index: 1, .. }
        ));
        assert_eq!(
            err.to_string(),
            "history entry 1 is call #1 github.close_issue({\"number\":4}), \
             but the script emitted call #1 github.close_issue({\"number\":5})"
        );
    }

    #[test]
    fn detects_an_extra_command() {
        let h = history(vec![
            started(),
            Event::Command(call(1, "a", json!({}))),
            done(1, json!(null)),
        ]);
        let mut cursor = ReplayCursor::new(&h).unwrap();
        cursor.on_command(&call(1, "a", json!({}))).unwrap();

        let err = cursor.on_command(&call(2, "b", json!({}))).unwrap_err();
        assert!(matches!(
            err,
            NonDeterminism::UnexpectedCommand { at: 2, .. }
        ));

        // Past the end of the history, any command is unexpected too.
        delivered(cursor.on_idle().unwrap());
        let err = cursor.on_command(&call(2, "b", json!({}))).unwrap_err();
        assert_eq!(
            err.to_string(),
            "the script emitted call #2 b({}), which the history does not contain (at entry 3)"
        );
    }

    #[test]
    fn detects_a_missing_command() {
        let h = history(vec![
            started(),
            Event::Command(Command::StartTimer {
                seq: Seq(1),
                duration_ms: 3_000,
                fire_at: Timestamp(4_000),
                callsite: None,
            }),
        ]);
        let mut cursor = ReplayCursor::new(&h).unwrap();
        let err = cursor.on_idle().unwrap_err();
        assert_eq!(
            err.to_string(),
            "history entry 1 is timer #1 (3000ms), but the script went idle without emitting it"
        );
    }

    #[test]
    fn skips_annotations_and_ignores_callsites() {
        let mut recorded = call(1, "a", json!({}));
        if let Command::ScheduleCall { callsite, .. } = &mut recorded {
            *callsite = Some(Callsite { line: 3, column: 9 });
        }
        let h = history(vec![
            started(),
            log("before"),
            Event::Command(recorded),
            log("after"),
            done(1, json!(1)),
            log("tail"),
        ]);
        let mut cursor = ReplayCursor::new(&h).unwrap();
        assert_eq!(
            cursor.on_command(&call(1, "a", json!({}))).unwrap(),
            Replayed::Matched { index: 2 }
        );
        assert!(matches!(
            cursor.on_idle().unwrap(),
            Next::Deliver { index: 4, .. }
        ));
        assert_eq!(cursor.on_idle().unwrap(), Next::End);
    }

    #[test]
    fn stops_where_the_original_run_was_aborted() {
        let abort = AbortReason {
            kind: AbortKind::LimitExceeded,
            message: "cpu time".into(),
        };
        let h = history(vec![
            started(),
            Event::Command(call(1, "a", json!({}))),
            done(1, json!(1)),
            Event::Lifecycle(Lifecycle::RunAborted(abort.clone())),
        ]);
        let mut cursor = ReplayCursor::new(&h).unwrap();
        cursor.on_command(&call(1, "a", json!({}))).unwrap();
        delivered(cursor.on_idle().unwrap());

        // The original run emitted commands here but they were dropped with the abort.
        assert_eq!(
            cursor.on_command(&call(2, "b", json!({}))).unwrap(),
            Replayed::Aborted {
                index: 3,
                reason: &abort
            }
        );
        assert_eq!(
            cursor.on_idle().unwrap(),
            Next::Aborted {
                index: 3,
                reason: &abort
            }
        );
        assert_eq!(cursor.aborted_here(), Some(&abort));
    }

    #[test]
    fn tells_whether_the_original_run_was_aborted_at_the_start() {
        let abort = AbortReason {
            kind: AbortKind::InvalidScript,
            message: "cannot import \"node:fs\"".into(),
        };
        let h = history(vec![
            started(),
            Event::Lifecycle(Lifecycle::RunAborted(abort.clone())),
        ]);
        assert_eq!(ReplayCursor::new(&h).unwrap().aborted_here(), Some(&abort));

        let h = history(vec![started(), Event::Command(call(1, "a", json!({})))]);
        assert_eq!(ReplayCursor::new(&h).unwrap().aborted_here(), None);
    }

    #[test]
    fn validates_the_history_first() {
        assert_eq!(ReplayCursor::new(&[]).err(), Some(HistoryError::Empty));
        assert_eq!(
            ReplayCursor::new(&history(vec![done(1, json!(1))])).err(),
            Some(HistoryError::MissingRunStarted)
        );
        let abort = Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
            kind: AbortKind::Internal,
            message: "boom".into(),
        }));
        assert_eq!(
            ReplayCursor::new(&history(vec![started(), abort, log("x")])).err(),
            Some(HistoryError::EntriesAfterEnd { index: 2 })
        );
    }
}
