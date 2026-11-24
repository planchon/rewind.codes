//! The events of a run's history (`archi/02-history.md`).
//!
//! Every event holds one event from four families:
//!
//! | Family | Events |
//! | --- | --- |
//! | [`Lifecycle`] | `run_started`, `run_aborted` |
//! | [`Command`] (emitted by the code) | `schedule_call`, `start_timer`, `cancel_timer`, `complete_run`, `fail_run` |
//! | [`Completion`] (delivered to the code) | `call_completed`, `call_failed`, `timer_fired`, `signal_received`, `cancel_requested` |
//! | [`Annotation`] (observability only) | `log`, `signal_awaited` |
//!
//! **Triggers** are `run_started` and every completion: a trigger wakes the code up, and
//! commands are what the code does in response.

use std::fmt;

use serde_json::Value;

use crate::digest::Digest;
use crate::ids::{Epoch, Seed, Seq};
use crate::payload::{Payload, truncate};
use crate::time::Timestamp;

/// Characters of a payload shown in messages read by models.
const PREVIEW_CHARS: usize = 120;

/// One event of a run's history.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEvent {
    /// Position in the history: starts at 0, contiguous.
    pub index: u64,
    /// When the event was committed: `max(now, previous recorded_at)`, so monotonic per run.
    /// Once a trigger is delivered, its `recorded_at` is the run's logical time.
    pub recorded_at: Timestamp,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Lifecycle(Lifecycle),
    /// Something the code asked for. Checked on replay.
    Command(Command),
    /// Something from the outside world, delivered to the code.
    Completion(Completion),
    /// Information about the run that does not affect it. Not checked on replay.
    Annotation(Annotation),
}

impl Event {
    /// Whether the event wakes the code up: `run_started` and every completion.
    ///
    /// Commit invariant: a trigger is committed in the same transaction as every event the
    /// code emitted in response to it.
    pub fn is_trigger(&self) -> bool {
        matches!(
            self,
            Event::Lifecycle(Lifecycle::RunStarted(_)) | Event::Completion(_)
        )
    }

    /// Whether the event ends the run: `complete_run`, `fail_run` or `run_aborted`.
    pub fn is_terminal(&self) -> bool {
        match self {
            Event::Command(command) => command.is_terminal(),
            Event::Lifecycle(Lifecycle::RunAborted(_)) => true,
            _ => false,
        }
    }

    /// The event's name in the history, such as `schedule_call`.
    pub fn name(&self) -> &'static str {
        match self {
            Event::Lifecycle(Lifecycle::RunStarted(_)) => "run_started",
            Event::Lifecycle(Lifecycle::RunAborted(_)) => "run_aborted",
            Event::Command(command) => command.name(),
            Event::Completion(completion) => completion.name(),
            Event::Annotation(annotation) => annotation.name(),
        }
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::Lifecycle(Lifecycle::RunStarted(started)) => write!(
                f,
                "run started (code {}, {}, input {})",
                started.code.short(),
                started.epoch,
                started.input.preview(PREVIEW_CHARS)
            ),
            Event::Lifecycle(Lifecycle::RunAborted(reason)) => write!(f, "run aborted ({reason})"),
            Event::Command(command) => command.fmt(f),
            Event::Completion(completion) => completion.fmt(f),
            Event::Annotation(annotation) => annotation.fmt(f),
        }
    }
}

// -------------------------------------------------------------------------------------------
// Lifecycle

#[derive(Clone, Debug, PartialEq)]
pub enum Lifecycle {
    RunStarted(RunStarted),
    /// The host ended the run: a limit was exceeded, the code could not be loaded, the
    /// cancellation grace ran out, a quarantined run was given up, or an internal error
    /// occurred. Unlike `fail_run`, the code did not produce it: replay stops here instead of
    /// reproducing it.
    RunAborted(AbortReason),
}

/// The first event of every history.
#[derive(Clone, Debug, PartialEq)]
pub struct RunStarted {
    /// Digest of the TypeScript source. A run always replays the exact code that produced it.
    pub code: Digest,
    /// Digest of the action set the run may call.
    pub action_set: Digest,
    pub input: Payload,
    /// Keys the sandbox's random generators.
    pub seed: Seed,
    /// The determinism epoch the run started under.
    pub epoch: Epoch,
    /// The run's deadline: when it was started plus its TTL.
    pub expires_at: Timestamp,
    pub mode: RunMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RunMode {
    /// Actions are really called and time is real.
    Live,
    /// Actions are answered by cassettes and handlers, and time is virtual.
    Test,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbortReason {
    pub kind: AbortKind,
    pub message: String,
}

impl fmt::Display for AbortReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AbortKind {
    /// A resource limit was exceeded (CPU time, heap, commands, history length, payload size).
    LimitExceeded,
    /// A quarantined run was given up, by an operator or at its deadline.
    NonDeterminism,
    /// The code did not end within its cancellation grace.
    Cancelled,
    /// The code could not be loaded.
    InvalidCode,
    /// A fault of the platform.
    Internal,
}

impl AbortKind {
    pub const ALL: [AbortKind; 5] = [
        AbortKind::LimitExceeded,
        AbortKind::NonDeterminism,
        AbortKind::Cancelled,
        AbortKind::InvalidCode,
        AbortKind::Internal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AbortKind::LimitExceeded => "limit_exceeded",
            AbortKind::NonDeterminism => "non_determinism",
            AbortKind::Cancelled => "cancelled",
            AbortKind::InvalidCode => "invalid_code",
            AbortKind::Internal => "internal",
        }
    }

    /// The kind [`as_str`](Self::as_str) spells `text`.
    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }
}

// -------------------------------------------------------------------------------------------
// Commands

#[derive(Clone, Debug, PartialEq)]
pub enum Command {
    ScheduleCall {
        seq: Seq,
        action: String,
        args: Payload,
        callsite: Option<Callsite>,
    },
    StartTimer {
        seq: Seq,
        duration_ms: u64,
        /// Logical time at emission plus the duration.
        fire_at: Timestamp,
        callsite: Option<Callsite>,
    },
    CancelTimer {
        seq: Seq,
    },
    /// The code succeeded. Emitted once `main` returned and no call is pending.
    CompleteRun {
        output: Payload,
    },
    /// The code failed: `main` threw, a rejection was left unhandled, or nothing can wake
    /// the code any more (`DeadlockError`). Emitted once no call is pending.
    FailRun {
        error: ErrorInfo,
    },
}

impl Command {
    /// The `seq` of a command that has one.
    pub fn seq(&self) -> Option<Seq> {
        match self {
            Command::ScheduleCall { seq, .. }
            | Command::StartTimer { seq, .. }
            | Command::CancelTimer { seq } => Some(*seq),
            Command::CompleteRun { .. } | Command::FailRun { .. } => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Command::CompleteRun { .. } | Command::FailRun { .. })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Command::ScheduleCall { .. } => "schedule_call",
            Command::StartTimer { .. } => "start_timer",
            Command::CancelTimer { .. } => "cancel_timer",
            Command::CompleteRun { .. } => "complete_run",
            Command::FailRun { .. } => "fail_run",
        }
    }

    /// Whether `other` is the same command for replay purposes: same kind, same `seq`, same
    /// action, same payloads (compared by digest).
    ///
    /// Callsites and stack traces are ignored: they locate a command in the source but don't
    /// change what it does, so moving code around is allowed. A `fail_run` matches on the
    /// error's name and message.
    pub fn matches(&self, other: &Command) -> bool {
        use Command::*;
        match (self, other) {
            (
                ScheduleCall {
                    seq: a_seq,
                    action: an_action,
                    args: a_args,
                    ..
                },
                ScheduleCall {
                    seq: b_seq,
                    action: b_action,
                    args: b_args,
                    ..
                },
            ) => a_seq == b_seq && an_action == b_action && a_args.same_as(b_args),
            (
                StartTimer {
                    seq: a_seq,
                    duration_ms: a_duration,
                    fire_at: a_fire_at,
                    ..
                },
                StartTimer {
                    seq: b_seq,
                    duration_ms: b_duration,
                    fire_at: b_fire_at,
                    ..
                },
            ) => a_seq == b_seq && a_duration == b_duration && a_fire_at == b_fire_at,
            (CancelTimer { seq: a }, CancelTimer { seq: b }) => a == b,
            (CompleteRun { output: a }, CompleteRun { output: b }) => a.same_as(b),
            (FailRun { error: a }, FailRun { error: b }) => {
                a.name == b.name && a.message == b.message
            }
            _ => false,
        }
    }
}

/// Short description, for error messages and traces read by models.
impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::ScheduleCall {
                seq, action, args, ..
            } => write!(f, "call {seq} {action}({})", args.preview(PREVIEW_CHARS)),
            Command::StartTimer {
                seq, duration_ms, ..
            } => write!(f, "timer {seq} ({duration_ms}ms)"),
            Command::CancelTimer { seq } => write!(f, "cancellation of timer {seq}"),
            Command::CompleteRun { output } => {
                write!(f, "run completion ({})", output.preview(PREVIEW_CHARS))
            }
            Command::FailRun { error } => write!(
                f,
                "run failure ({}: {})",
                error.name,
                truncate(&error.message, PREVIEW_CHARS)
            ),
        }
    }
}

/// What a failed code reports. Besides what `main` throws, the runtime fails a code with
/// the rejection it left unhandled, or with the name `DeadlockError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ErrorInfo {
    pub name: String,
    pub message: String,
    pub stack: Option<String>,
}

impl ErrorInfo {
    /// The name of the error a run fails with when nothing can wake the code any more.
    pub const DEADLOCK: &'static str = "DeadlockError";
    /// The name of the error that rejects every pending await on cancellation. A run that
    /// fails with it is `cancelled`.
    pub const CANCELLED: &'static str = "CancelledError";
    /// The name of the error a failed call rejects with.
    pub const ACTION_ERROR: &'static str = "ActionError";
    /// Its name under the determinism epoch 1, which the runs started before the rename release
    /// recorded (`archi/03-execution.md`, "Determinism epoch").
    pub const TOOL_ERROR: &'static str = "ToolError";

    /// The message of the `ActionError` the runtime builds from a failed call of `action`:
    /// `{action} failed: {message}`, where `message` is the recorded failure's.
    pub fn action_error_message(action: &str, failure: &ActionFailure) -> String {
        format!("{action} failed: {}", failure.message)
    }

    /// The call whose uncaught `ActionError` this error is, read from the run's history.
    ///
    /// The error records no `seq`: the runtime builds it from the call's recorded
    /// `call_failed` completion and the action its `schedule_call` names. The call is
    /// the last recorded failure that builds exactly this error. `None` when the error is not an
    /// `ActionError` (or a `ToolError`, its name for a run of epoch 1), or when no recorded
    /// failure builds it (the code threw one it made itself from another error's message, say).
    pub fn failed_call(&self, history: &[HistoryEvent]) -> Option<Seq> {
        if self.name != Self::ACTION_ERROR && self.name != Self::TOOL_ERROR {
            return None;
        }
        let actions: std::collections::BTreeMap<Seq, &str> = history
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::Command(Command::ScheduleCall { seq, action, .. }) => {
                    Some((*seq, action.as_str()))
                }
                _ => None,
            })
            .collect();
        history.iter().rev().find_map(|entry| match &entry.event {
            Event::Completion(Completion::CallFailed { seq, error })
                if actions.get(seq).is_some_and(|action| {
                    Self::action_error_message(action, error) == self.message
                }) =>
            {
                Some(*seq)
            }
            _ => None,
        })
    }
}

/// Position in the code's TypeScript source, 1-based.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Callsite {
    pub line: u32,
    pub column: u32,
}

impl fmt::Display for Callsite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "L{}:{}", self.line, self.column)
    }
}

// -------------------------------------------------------------------------------------------
// Completions

#[derive(Clone, Debug, PartialEq)]
pub enum Completion {
    CallCompleted {
        seq: Seq,
        result: Payload,
    },
    CallFailed {
        seq: Seq,
        error: ActionFailure,
    },
    TimerFired {
        seq: Seq,
    },
    /// Kept in the name's mailbox until the code awaits it, first in first out.
    SignalReceived {
        name: String,
        payload: Payload,
    },
    /// Rejects every pending await with `CancelledError`.
    CancelRequested {
        reason: CancelReason,
    },
}

impl Completion {
    pub fn name(&self) -> &'static str {
        match self {
            Completion::CallCompleted { .. } => "call_completed",
            Completion::CallFailed { .. } => "call_failed",
            Completion::TimerFired { .. } => "timer_fired",
            Completion::SignalReceived { .. } => "signal_received",
            Completion::CancelRequested { .. } => "cancel_requested",
        }
    }
}

impl fmt::Display for Completion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Completion::CallCompleted { seq, result } => {
                write!(
                    f,
                    "result of call {seq} ({})",
                    result.preview(PREVIEW_CHARS)
                )
            }
            Completion::CallFailed { seq, error } => write!(
                f,
                "failure of call {seq} ({}: {})",
                error.kind.as_str(),
                truncate(&error.message, PREVIEW_CHARS)
            ),
            Completion::TimerFired { seq } => write!(f, "timer {seq} fired"),
            Completion::SignalReceived { name, payload } => write!(
                f,
                "signal {name:?} received ({})",
                payload.preview(PREVIEW_CHARS)
            ),
            Completion::CancelRequested { reason } => {
                write!(f, "cancellation requested ({reason})")
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActionFailure {
    pub kind: ActionFailureKind,
    pub message: String,
    pub data: Option<Value>,
}

/// The outcome of a call: what the dispatcher or the harness's worker reports, and what a
/// cassette replays. It reaches the code as a [`Completion`] of the call's `seq`.
#[derive(Clone, Debug, PartialEq)]
pub enum ActionOutcome {
    Result(Value),
    Failure(ActionFailure),
}

impl ActionOutcome {
    /// The completion that delivers this outcome to the call `seq`.
    pub fn into_completion(self, seq: Seq) -> Completion {
        match self {
            ActionOutcome::Result(result) => Completion::CallCompleted {
                seq,
                result: Payload::Value(result),
            },
            ActionOutcome::Failure(error) => Completion::CallFailed { seq, error },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionFailureKind {
    /// The action ran and reported an error.
    Error,
    /// The action could not be reached, after retries.
    Unavailable,
    /// The action did not answer in time.
    Timeout,
    /// The call may or may not have taken effect, and the action's policy forbids retrying it.
    OutcomeUnknown,
}

impl ActionFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ActionFailureKind::Error => "error",
            ActionFailureKind::Unavailable => "unavailable",
            ActionFailureKind::Timeout => "timeout",
            ActionFailureKind::OutcomeUnknown => "outcome_unknown",
        }
    }
}

/// Why a run is being cancelled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CancelReason {
    /// Someone asked, with an optional message.
    Requested(Option<String>),
    /// The run's deadline (`expires_at`) has passed.
    Ttl,
}

impl fmt::Display for CancelReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CancelReason::Requested(Some(message)) => f.write_str(message),
            CancelReason::Requested(None) => f.write_str("requested"),
            CancelReason::Ttl => f.write_str("the run's deadline has passed"),
        }
    }
}

// -------------------------------------------------------------------------------------------
// Annotations

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Annotation {
    Log {
        level: LogLevel,
        message: String,
        callsite: Option<Callsite>,
    },
    /// The code started waiting for a signal whose mailbox was empty.
    SignalAwaited {
        name: String,
        /// The `seq` of the timer that implements the timeout, if any.
        timeout: Option<Seq>,
        callsite: Option<Callsite>,
    },
}

impl Annotation {
    pub fn name(&self) -> &'static str {
        match self {
            Annotation::Log { .. } => "log",
            Annotation::SignalAwaited { .. } => "signal_awaited",
        }
    }
}

impl fmt::Display for Annotation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Annotation::Log { level, message, .. } => {
                write!(f, "{} {}", level.as_str(), truncate(message, PREVIEW_CHARS))
            }
            Annotation::SignalAwaited {
                name,
                timeout: Some(timer),
                ..
            } => write!(f, "waiting for signal {name:?} (timeout timer {timer})"),
            Annotation::SignalAwaited {
                name,
                timeout: None,
                ..
            } => write!(f, "waiting for signal {name:?}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn call(line: u32, repo: &str) -> Command {
        Command::ScheduleCall {
            seq: Seq(1),
            action: "github.list_issues".into(),
            args: json!({ "repo": repo, "state": "open" }).into(),
            callsite: Some(Callsite { line, column: 1 }),
        }
    }

    #[test]
    fn matching_ignores_callsites_but_not_args() {
        assert!(call(5, "acme/api").matches(&call(9, "acme/api")));
        assert!(!call(5, "acme/api").matches(&call(5, "acme/web")));

        let digest_only = match call(5, "acme/api") {
            Command::ScheduleCall { args, .. } => Command::ScheduleCall {
                seq: Seq(1),
                action: "github.list_issues".into(),
                args: Payload::Digest(args.digest()),
                callsite: None,
            },
            _ => unreachable!(),
        };
        assert!(digest_only.matches(&call(7, "acme/api")));
    }

    #[test]
    fn matching_compares_kinds_and_seqs() {
        let timer = |seq, duration_ms| Command::StartTimer {
            seq: Seq(seq),
            duration_ms,
            fire_at: Timestamp(duration_ms),
            callsite: None,
        };
        assert!(timer(1, 10).matches(&timer(1, 10)));
        assert!(!timer(1, 10).matches(&timer(2, 10)));
        assert!(!timer(1, 10).matches(&timer(1, 20)));
        assert!(!timer(1, 10).matches(&Command::CancelTimer { seq: Seq(1) }));
    }

    #[test]
    fn failures_match_on_name_and_message_only() {
        let fail = |message: &str, stack: &str| Command::FailRun {
            error: ErrorInfo {
                name: "TypeError".into(),
                message: message.into(),
                stack: Some(stack.into()),
            },
        };
        assert!(
            fail("x is undefined", "at main (workflow.ts:3:1)")
                .matches(&fail("x is undefined", "at main (workflow.ts:4:1)"))
        );
        assert!(!fail("x is undefined", "").matches(&fail("y is undefined", "")));
    }

    /// The call an uncaught `ActionError` came from is found through its recorded completion, not
    /// by comparing the error's message with the action's: the runtime prefixes the action's name.
    #[test]
    fn an_uncaught_action_error_names_its_call() {
        let entry = |index: u64, event: Event| HistoryEvent {
            index,
            recorded_at: Timestamp(index),
            event,
        };
        let schedule = |seq: u32, action: &str| {
            Event::Command(Command::ScheduleCall {
                seq: Seq(seq),
                action: action.into(),
                args: json!({}).into(),
                callsite: None,
            })
        };
        let failed = |seq: u32, message: &str| {
            Event::Completion(Completion::CallFailed {
                seq: Seq(seq),
                error: ActionFailure {
                    kind: ActionFailureKind::Error,
                    message: message.into(),
                    data: None,
                },
            })
        };
        // The code caught the first failure, then left the third uncaught.
        let history = [
            entry(1, schedule(1, "payments.charge")),
            entry(2, failed(1, "card declined")),
            entry(3, schedule(2, "audit.record")),
            entry(4, failed(2, "card declined")),
            entry(5, schedule(3, "payments.charge")),
            entry(6, failed(3, "card expired")),
        ];
        let error = |name: &str, message: &str| ErrorInfo {
            name: name.into(),
            message: message.into(),
            stack: None,
        };
        let uncaught = |message| error(ErrorInfo::ACTION_ERROR, message).failed_call(&history);
        assert_eq!(
            uncaught("payments.charge failed: card expired"),
            Some(Seq(3))
        );
        assert_eq!(
            uncaught("payments.charge failed: card declined"),
            Some(Seq(1))
        );
        assert_eq!(uncaught("audit.record failed: card declined"), Some(Seq(2)));
        // The action's own message is not the error's.
        assert_eq!(uncaught("card expired"), None);
        assert_eq!(
            error("Error", "payments.charge failed: card expired").failed_call(&history),
            None,
            "not an ActionError"
        );
        assert_eq!(
            ErrorInfo::action_error_message(
                "github.close_issue",
                &ActionFailure {
                    kind: ActionFailureKind::Timeout,
                    message: "no answer in 30s".into(),
                    data: None,
                }
            ),
            "github.close_issue failed: no answer in 30s"
        );
    }

    #[test]
    fn display_truncates_long_args() {
        let command = Command::ScheduleCall {
            seq: Seq(2),
            action: "slack.post_message".into(),
            args: json!({ "text": "x".repeat(500) }).into(),
            callsite: None,
        };
        let text = command.to_string();
        assert!(text.starts_with("call #2 slack.post_message({\"text\":\"xxx"));
        assert!(text.ends_with("…)"));
        assert!(text.chars().count() < 160);
    }

    #[test]
    fn triggers_and_terminal_events() {
        let completion = Event::Completion(Completion::TimerFired { seq: Seq(1) });
        assert!(completion.is_trigger());
        assert!(!completion.is_terminal());
        let complete = Event::Command(Command::CompleteRun {
            output: json!(null).into(),
        });
        assert!(!complete.is_trigger());
        assert!(complete.is_terminal());
        let aborted = Event::Lifecycle(Lifecycle::RunAborted(AbortReason {
            kind: AbortKind::Internal,
            message: "boom".into(),
        }));
        assert!(aborted.is_terminal());
        assert_eq!(aborted.to_string(), "run aborted (internal: boom)");
    }
}
