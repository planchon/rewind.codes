//! The events of a run's journal (`archi/02-journal.md`).
//!
//! Every entry holds one event from four families:
//!
//! | Family | Events |
//! | --- | --- |
//! | [`Lifecycle`] | `run_started`, `run_aborted` |
//! | [`Command`] (emitted by the script) | `schedule_tool_call`, `start_timer`, `cancel_timer`, `complete_run`, `fail_run` |
//! | [`Completion`] (delivered to the script) | `tool_call_completed`, `tool_call_failed`, `timer_fired`, `signal_received`, `cancel_requested` |
//! | [`Annotation`] (observability only) | `log`, `signal_awaited` |
//!
//! **Triggers** are `run_started` and every completion: a trigger wakes the script up, and
//! commands are what the script does in response.

use std::fmt;

use serde_json::Value;

use crate::digest::Digest;
use crate::ids::{Epoch, Seed, Seq};
use crate::payload::{Payload, truncate};
use crate::time::Timestamp;

/// Characters of a payload shown in messages read by models.
const PREVIEW_CHARS: usize = 120;

/// One entry of a run's journal.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoryEntry {
    /// Position in the history: starts at 0, contiguous.
    pub index: u64,
    /// When the entry was committed: `max(now, previous recorded_at)`, so monotonic per run.
    /// Once a trigger is delivered, its `recorded_at` is the run's logical time.
    pub recorded_at: Timestamp,
    pub event: Event,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Lifecycle(Lifecycle),
    /// Something the script asked for. Checked on replay.
    Command(Command),
    /// Something from the outside world, delivered to the script.
    Completion(Completion),
    /// Information about the run that does not affect it. Not checked on replay.
    Annotation(Annotation),
}

impl Event {
    /// Whether the event wakes the script up: `run_started` and every completion.
    ///
    /// Commit invariant: a trigger is committed in the same transaction as every entry the
    /// script emitted in response to it.
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

    /// The event's name in the journal, such as `schedule_tool_call`.
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
                "run started (script {}, {}, input {})",
                started.script.short(),
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
    /// The host ended the run: a limit was exceeded, the script could not be loaded, the
    /// cancellation grace ran out, a quarantined run was given up, or an internal error
    /// occurred. Unlike `fail_run`, the script did not produce it: replay stops here instead of
    /// reproducing it.
    RunAborted(AbortReason),
}

/// The first entry of every history.
#[derive(Clone, Debug, PartialEq)]
pub struct RunStarted {
    /// Digest of the TypeScript source. A run always replays the exact code that produced it.
    pub script: Digest,
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
    /// Tools are really called and time is real.
    Live,
    /// Tools are answered by cassettes and mocks, and time is virtual.
    Mock,
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
    /// The script did not end within its cancellation grace.
    Cancelled,
    /// The script could not be loaded.
    InvalidScript,
    /// A fault of the platform.
    Internal,
}

impl AbortKind {
    pub const ALL: [AbortKind; 5] = [
        AbortKind::LimitExceeded,
        AbortKind::NonDeterminism,
        AbortKind::Cancelled,
        AbortKind::InvalidScript,
        AbortKind::Internal,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            AbortKind::LimitExceeded => "limit_exceeded",
            AbortKind::NonDeterminism => "non_determinism",
            AbortKind::Cancelled => "cancelled",
            AbortKind::InvalidScript => "invalid_script",
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
    ScheduleToolCall {
        seq: Seq,
        tool: String,
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
    /// The script succeeded. Emitted once `main` returned and no tool call is pending.
    CompleteRun {
        output: Payload,
    },
    /// The script failed: `main` threw, a rejection was left unhandled, or nothing can wake
    /// the script any more (`DeadlockError`). Emitted once no tool call is pending.
    FailRun {
        error: ScriptError,
    },
}

impl Command {
    /// The `seq` of a command that has one.
    pub fn seq(&self) -> Option<Seq> {
        match self {
            Command::ScheduleToolCall { seq, .. }
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
            Command::ScheduleToolCall { .. } => "schedule_tool_call",
            Command::StartTimer { .. } => "start_timer",
            Command::CancelTimer { .. } => "cancel_timer",
            Command::CompleteRun { .. } => "complete_run",
            Command::FailRun { .. } => "fail_run",
        }
    }

    /// Whether `other` is the same command for replay purposes: same kind, same `seq`, same
    /// tool, same payloads (compared by digest).
    ///
    /// Callsites and stack traces are ignored: they locate a command in the source but don't
    /// change what it does, so moving code around is allowed. A `fail_run` matches on the
    /// error's name and message.
    pub fn matches(&self, other: &Command) -> bool {
        use Command::*;
        match (self, other) {
            (
                ScheduleToolCall {
                    seq: a_seq,
                    tool: a_tool,
                    args: a_args,
                    ..
                },
                ScheduleToolCall {
                    seq: b_seq,
                    tool: b_tool,
                    args: b_args,
                    ..
                },
            ) => a_seq == b_seq && a_tool == b_tool && a_args.same_as(b_args),
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
            Command::ScheduleToolCall {
                seq, tool, args, ..
            } => write!(f, "tool call {seq} {tool}({})", args.preview(PREVIEW_CHARS)),
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

/// What a failed script reports. Besides what `main` throws, the runtime fails a script with
/// the rejection it left unhandled, or with the name `DeadlockError`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScriptError {
    pub name: String,
    pub message: String,
    pub stack: Option<String>,
}

impl ScriptError {
    /// The name of the error a run fails with when nothing can wake the script any more.
    pub const DEADLOCK: &'static str = "DeadlockError";
    /// The name of the error that rejects every pending await on cancellation. A run that
    /// fails with it is `cancelled`.
    pub const CANCELLED: &'static str = "CancelledError";
    /// The name of the error a failed tool call rejects with.
    pub const TOOL_ERROR: &'static str = "ToolError";

    /// The message of the `ToolError` the runtime builds from a failed call of `tool`:
    /// `{tool} failed: {message}`, where `message` is the recorded failure's.
    pub fn tool_error_message(tool: &str, failure: &ToolFailure) -> String {
        format!("{tool} failed: {}", failure.message)
    }

    /// The tool call whose uncaught `ToolError` this error is, read from the run's history.
    ///
    /// The error records no `seq`: the runtime builds it from the call's recorded
    /// `tool_call_failed` completion and the tool its `schedule_tool_call` names. The call is
    /// the last recorded failure that builds exactly this error. `None` when the error is not a
    /// `ToolError`, or when no recorded failure builds it (the script threw one it made itself
    /// from another error's message, say).
    pub fn failed_call(&self, history: &[HistoryEntry]) -> Option<Seq> {
        if self.name != Self::TOOL_ERROR {
            return None;
        }
        let tools: std::collections::BTreeMap<Seq, &str> = history
            .iter()
            .filter_map(|entry| match &entry.event {
                Event::Command(Command::ScheduleToolCall { seq, tool, .. }) => {
                    Some((*seq, tool.as_str()))
                }
                _ => None,
            })
            .collect();
        history.iter().rev().find_map(|entry| match &entry.event {
            Event::Completion(Completion::ToolCallFailed { seq, error })
                if tools
                    .get(seq)
                    .is_some_and(|tool| Self::tool_error_message(tool, error) == self.message) =>
            {
                Some(*seq)
            }
            _ => None,
        })
    }
}

/// Position in the script's TypeScript source, 1-based.
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
    ToolCallCompleted {
        seq: Seq,
        result: Payload,
    },
    ToolCallFailed {
        seq: Seq,
        error: ToolFailure,
    },
    TimerFired {
        seq: Seq,
    },
    /// Kept in the name's mailbox until the script awaits it, first in first out.
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
            Completion::ToolCallCompleted { .. } => "tool_call_completed",
            Completion::ToolCallFailed { .. } => "tool_call_failed",
            Completion::TimerFired { .. } => "timer_fired",
            Completion::SignalReceived { .. } => "signal_received",
            Completion::CancelRequested { .. } => "cancel_requested",
        }
    }
}

impl fmt::Display for Completion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Completion::ToolCallCompleted { seq, result } => {
                write!(
                    f,
                    "result of tool call {seq} ({})",
                    result.preview(PREVIEW_CHARS)
                )
            }
            Completion::ToolCallFailed { seq, error } => write!(
                f,
                "failure of tool call {seq} ({}: {})",
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
pub struct ToolFailure {
    pub kind: ToolFailureKind,
    pub message: String,
    pub data: Option<Value>,
}

/// The outcome of a tool call: what the dispatcher or the harness's worker reports, and what a
/// cassette replays. It reaches the script as a [`Completion`] of the call's `seq`.
#[derive(Clone, Debug, PartialEq)]
pub enum ToolOutcome {
    Result(Value),
    Failure(ToolFailure),
}

impl ToolOutcome {
    /// The completion that delivers this outcome to the call `seq`.
    pub fn into_completion(self, seq: Seq) -> Completion {
        match self {
            ToolOutcome::Result(result) => Completion::ToolCallCompleted {
                seq,
                result: Payload::Value(result),
            },
            ToolOutcome::Failure(error) => Completion::ToolCallFailed { seq, error },
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ToolFailureKind {
    /// The tool ran and reported an error.
    Error,
    /// The tool could not be reached, after retries.
    Unavailable,
    /// The tool did not answer in time.
    Timeout,
    /// The call may or may not have taken effect, and the tool's policy forbids retrying it.
    OutcomeUnknown,
}

impl ToolFailureKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ToolFailureKind::Error => "error",
            ToolFailureKind::Unavailable => "unavailable",
            ToolFailureKind::Timeout => "timeout",
            ToolFailureKind::OutcomeUnknown => "outcome_unknown",
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
    /// The script started waiting for a signal whose mailbox was empty.
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
        Command::ScheduleToolCall {
            seq: Seq(1),
            tool: "github.list_issues".into(),
            args: json!({ "repo": repo, "state": "open" }).into(),
            callsite: Some(Callsite { line, column: 1 }),
        }
    }

    #[test]
    fn matching_ignores_callsites_but_not_args() {
        assert!(call(5, "acme/api").matches(&call(9, "acme/api")));
        assert!(!call(5, "acme/api").matches(&call(5, "acme/web")));

        let digest_only = match call(5, "acme/api") {
            Command::ScheduleToolCall { args, .. } => Command::ScheduleToolCall {
                seq: Seq(1),
                tool: "github.list_issues".into(),
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
            error: ScriptError {
                name: "TypeError".into(),
                message: message.into(),
                stack: Some(stack.into()),
            },
        };
        assert!(
            fail("x is undefined", "at main (script.ts:3:1)")
                .matches(&fail("x is undefined", "at main (script.ts:4:1)"))
        );
        assert!(!fail("x is undefined", "").matches(&fail("y is undefined", "")));
    }

    /// The call an uncaught `ToolError` came from is found through its recorded completion, not
    /// by comparing the error's message with the tool's: the runtime prefixes the tool's name.
    #[test]
    fn an_uncaught_tool_error_names_its_call() {
        let entry = |index: u64, event: Event| HistoryEntry {
            index,
            recorded_at: Timestamp(index),
            event,
        };
        let schedule = |seq: u32, tool: &str| {
            Event::Command(Command::ScheduleToolCall {
                seq: Seq(seq),
                tool: tool.into(),
                args: json!({}).into(),
                callsite: None,
            })
        };
        let failed = |seq: u32, message: &str| {
            Event::Completion(Completion::ToolCallFailed {
                seq: Seq(seq),
                error: ToolFailure {
                    kind: ToolFailureKind::Error,
                    message: message.into(),
                    data: None,
                },
            })
        };
        // The script caught the first failure, then left the third uncaught.
        let history = [
            entry(1, schedule(1, "payments.charge")),
            entry(2, failed(1, "card declined")),
            entry(3, schedule(2, "audit.record")),
            entry(4, failed(2, "card declined")),
            entry(5, schedule(3, "payments.charge")),
            entry(6, failed(3, "card expired")),
        ];
        let error = |name: &str, message: &str| ScriptError {
            name: name.into(),
            message: message.into(),
            stack: None,
        };
        let uncaught = |message| error(ScriptError::TOOL_ERROR, message).failed_call(&history);
        assert_eq!(
            uncaught("payments.charge failed: card expired"),
            Some(Seq(3))
        );
        assert_eq!(
            uncaught("payments.charge failed: card declined"),
            Some(Seq(1))
        );
        assert_eq!(uncaught("audit.record failed: card declined"), Some(Seq(2)));
        // The tool's own message is not the error's.
        assert_eq!(uncaught("card expired"), None);
        assert_eq!(
            error("Error", "payments.charge failed: card expired").failed_call(&history),
            None,
            "not a ToolError"
        );
        assert_eq!(
            ScriptError::tool_error_message(
                "github.close_issue",
                &ToolFailure {
                    kind: ToolFailureKind::Timeout,
                    message: "no answer in 30s".into(),
                    data: None,
                }
            ),
            "github.close_issue failed: no answer in 30s"
        );
    }

    #[test]
    fn display_truncates_long_args() {
        let command = Command::ScheduleToolCall {
            seq: Seq(2),
            tool: "slack.post_message".into(),
            args: json!({ "text": "x".repeat(500) }).into(),
            callsite: None,
        };
        let text = command.to_string();
        assert!(text.starts_with("tool call #2 slack.post_message({\"text\":\"xxx"));
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
