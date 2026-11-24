//! The LLM trace of a run (`archi/05-testing-and-observability.md`, "LLM trace and metrics"):
//! `GET /v1/runs/{id}/trace?format=llm`, `rewind trace <run> [--event n]`, and the trace of a
//! scenario's report. It is rendered from the history alone, to be read by a model: one line per
//! call, timer, signal and log, with its line in `workflow.ts` and its outcome folded in;
//! payloads are truncated, and any event can be shown in full.
//!
//! ```text
//! run 0198…0001  suspended · waiting for signal "approval" (timeout in 2d 23h)
//! code 21a0270b7f66 · action set 5598149567a4 · started 2026-01-01T00:00:00Z
//!
//!   idx  at       event                                                    line  outcome
//!     1  +0.0s    #1 github.list_issues({"repo":"acme/api","state":"open"})  L5   → 37 items · 12.4 KB · 310ms
//!     4  +0.3s    timer #2 3d (timeout of signal "approval")              L8   → pending
//! ```

use std::collections::BTreeMap;
use std::fmt::Write as _;

use rewind_core::{
    Annotation, Command, Completion, Digest, Event, HistoryEvent, JsonValue, Lifecycle, Payload,
    RunId, RunState, RunStatus, Seq, Timestamp, canonical_json,
};

use crate::ports::{ForkParent, PayloadRecord, Quarantine, RunHead};

/// Characters of a payload in a trace line.
const PREVIEW_CHARS: usize = 60;
/// Characters of a log message in a trace line.
const MESSAGE_CHARS: usize = 100;
/// Width of the event column.
const EVENT_WIDTH: usize = 58;

/// What a trace is rendered from.
#[derive(Debug, Clone, Copy)]
pub struct TraceInput<'a> {
    pub head: &'a RunHead,
    /// The history of the run's attempt.
    pub history: &'a [HistoryEvent],
    /// The records of the payloads the history holds by digest: their size and preview.
    pub payloads: &'a BTreeMap<Digest, PayloadRecord>,
    /// For the time left before a deadline.
    pub now: Timestamp,
}

/// The trace of a run, from its head and the history of its attempt.
pub fn render_trace(input: TraceInput<'_>) -> String {
    let TraceInput {
        head,
        history,
        payloads,
        now,
    } = input;
    let shown = Shown {
        run: head.run.id,
        code: head.code,
        action_set: head.action_set,
        status: head.status,
        quarantine: head.quarantine.as_ref(),
        parent: head.metadata.parent.as_ref(),
    };
    render(&shown, history, payloads, now)
}

/// What a trace shows of a run besides its history.
struct Shown<'a> {
    run: RunId,
    code: Digest,
    action_set: Digest,
    status: RunStatus,
    quarantine: Option<&'a Quarantine>,
    parent: Option<&'a ForkParent>,
}

fn render(
    record: &Shown<'_>,
    history: &[HistoryEvent],
    payloads: &BTreeMap<Digest, PayloadRecord>,
    now: Timestamp,
) -> String {
    let mut out = String::new();
    let state = RunState::from_history(history).ok();
    let _ = writeln!(
        out,
        "run {}  {}",
        record.run,
        headline(record, state.as_ref(), history, now)
    );
    let started_at = history.first().map(|entry| entry.recorded_at);
    let mut second = format!(
        "code {} · action set {}",
        record.code.short(),
        record.action_set.short()
    );
    match started_at {
        Some(at) => {
            let _ = write!(second, " · started {}", rfc3339(at));
        }
        None => second.push_str(" · not started"),
    }
    if let Some(parent) = record.parent {
        let _ = write!(second, " · forked from {}", parent.run);
        if let Some(at) = parent.at {
            let _ = write!(second, " (live from event {at})");
        }
    }
    let _ = writeln!(out, "{second}");
    let Some(started_at) = started_at else {
        return out;
    };
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "  idx  at        {:<EVENT_WIDTH$}  line   outcome",
        "event"
    );

    let outcomes = Outcomes::of(history);
    for entry in history {
        let Some(line) = describe(entry, &outcomes, payloads) else {
            continue;
        };
        let _ = writeln!(
            out,
            "{:>5}  {:<8}  {:<EVENT_WIDTH$}  {:<5}  {}",
            entry.index,
            elapsed(entry.recorded_at, started_at),
            line.event,
            line.line.map_or(String::new(), |line| format!("L{line}")),
            line.outcome.unwrap_or_default()
        );
    }
    // Trim the padding of lines without a line or an outcome.
    out.lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// One event in full, with its payload's value when it has one.
pub fn render_event(
    entry: &HistoryEvent,
    value: Option<&JsonValue>,
    started_at: Timestamp,
) -> String {
    let mut out = format!(
        "event {} · {} · {} ({})\n",
        entry.index,
        entry.event.name(),
        elapsed(entry.recorded_at, started_at),
        rfc3339(entry.recorded_at)
    );
    let _ = writeln!(out, "{}", entry.event);
    match &entry.event {
        Event::Command(Command::FailRun { error }) => {
            if let Some(stack) = &error.stack {
                let _ = writeln!(out, "{stack}");
            }
        }
        Event::Completion(Completion::CallFailed { error, .. }) => {
            let _ = writeln!(out, "{}: {}", error.kind.as_str(), error.message);
            if let Some(data) = &error.data {
                let _ = writeln!(out, "{}", pretty(data));
            }
        }
        _ => {}
    }
    if let Some(value) = value {
        let label = match &entry.event {
            Event::Lifecycle(Lifecycle::RunStarted(_)) => "input",
            Event::Command(Command::ScheduleCall { .. }) => "args",
            Event::Command(Command::CompleteRun { .. }) => "output",
            Event::Completion(Completion::CallCompleted { .. }) => "result",
            _ => "payload",
        };
        let size = canonical_json(value).len() as u64;
        let _ = writeln!(out, "{label} ({}):\n{}", bytes(size), pretty(value));
    }
    out
}

fn pretty(value: &JsonValue) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// The run's state in a few words: its outcome, or what it waits for.
fn headline(
    record: &Shown<'_>,
    state: Option<&RunState>,
    history: &[HistoryEvent],
    now: Timestamp,
) -> String {
    let mut headline = match state.map(RunState::outcome) {
        _ if history.is_empty() => "pending · not activated yet".to_owned(),
        Some(Some(rewind_core::RunOutcome::Completed { .. })) => "completed".to_owned(),
        Some(Some(rewind_core::RunOutcome::Failed { error }))
            if record.status == RunStatus::Cancelled
                || error.name == rewind_core::ScriptError::CANCELLED =>
        {
            format!("cancelled · {}", truncate(&error.message, MESSAGE_CHARS))
        }
        Some(Some(rewind_core::RunOutcome::Failed { error })) => format!(
            "failed · {}: {}",
            error.name,
            truncate(&error.message, MESSAGE_CHARS)
        ),
        Some(Some(rewind_core::RunOutcome::Aborted { reason })) => {
            format!("aborted · {}", truncate(&reason.to_string(), MESSAGE_CHARS))
        }
        Some(None) => waiting(state, now),
        None => "invalid history".to_owned(),
    };
    if let Some(quarantine) = record.quarantine {
        let kind = match quarantine.kind {
            crate::ports::QuarantineKind::NonDeterminism => "non_determinism",
            crate::ports::QuarantineKind::Failures => "failures",
        };
        let _ = write!(
            headline,
            " · quarantined ({kind}): {}",
            truncate(&quarantine.reason, MESSAGE_CHARS)
        );
    }
    headline
}

/// What an active run waits for.
fn waiting(state: Option<&RunState>, now: Timestamp) -> String {
    let Some(state) = state else {
        return "active".to_owned();
    };
    let calls = state.pending_calls();
    if !calls.is_empty() {
        let names: Vec<String> = calls
            .iter()
            .map(|(seq, action)| format!("{action} {seq}"))
            .collect();
        return format!("running · waiting for {}", names.join(", "));
    }
    if let Some(wait) = state.awaited_signals().first() {
        let mut text = format!("suspended · waiting for signal {:?}", wait.name);
        if let Some(fire_at) = wait
            .timeout
            .and_then(|seq| state.pending_timers().get(&seq).copied())
        {
            let _ = write!(text, " (timeout in {})", left(fire_at, now));
        }
        return text;
    }
    if let Some(fire_at) = state.pending_timers().values().min() {
        return format!(
            "sleeping · until {} (in {})",
            rfc3339(*fire_at),
            left(*fire_at, now)
        );
    }
    "active".to_owned()
}

fn left(at: Timestamp, now: Timestamp) -> String {
    duration(at.as_millis().saturating_sub(now.as_millis()))
}

/// How each command ended, by `seq`, and how each signal wait ended, by index.
struct Outcomes<'h> {
    calls: BTreeMap<Seq, &'h HistoryEvent>,
    timers: BTreeMap<Seq, &'h HistoryEvent>,
    /// Timers that implement the timeout of a signal wait: seq → signal name.
    timeouts: BTreeMap<Seq, &'h str>,
    waits: BTreeMap<u64, &'h HistoryEvent>,
    /// The run has ended: what is still pending was dropped with it.
    ended: bool,
}

impl<'h> Outcomes<'h> {
    fn of(history: &'h [HistoryEvent]) -> Self {
        let mut outcomes = Self {
            calls: BTreeMap::new(),
            timers: BTreeMap::new(),
            timeouts: BTreeMap::new(),
            waits: BTreeMap::new(),
            ended: history
                .last()
                .is_some_and(|entry| entry.event.is_terminal()),
        };
        let mut open_waits: Vec<(u64, &str, Option<Seq>)> = Vec::new();
        for entry in history {
            match &entry.event {
                Event::Completion(
                    Completion::CallCompleted { seq, .. } | Completion::CallFailed { seq, .. },
                ) => {
                    outcomes.calls.insert(*seq, entry);
                }
                Event::Completion(Completion::TimerFired { seq })
                | Event::Command(Command::CancelTimer { seq }) => {
                    outcomes.timers.entry(*seq).or_insert(entry);
                    if matches!(entry.event, Event::Completion(_))
                        && let Some(position) = open_waits
                            .iter()
                            .position(|(_, _, timeout)| *timeout == Some(*seq))
                    {
                        let (index, ..) = open_waits.remove(position);
                        outcomes.waits.insert(index, entry);
                    }
                }
                Event::Annotation(Annotation::SignalAwaited { name, timeout, .. }) => {
                    open_waits.push((entry.index, name, *timeout));
                    if let Some(timeout) = timeout {
                        outcomes.timeouts.insert(*timeout, name);
                    }
                }
                Event::Completion(Completion::SignalReceived { name, .. }) => {
                    if let Some(position) = open_waits.iter().position(|(_, n, _)| n == name) {
                        let (index, ..) = open_waits.remove(position);
                        outcomes.waits.insert(index, entry);
                    }
                }
                _ => {}
            }
        }
        outcomes
    }
}

struct Line {
    event: String,
    line: Option<u32>,
    outcome: Option<String>,
}

/// The line of an event, or `None` for an event folded into another one's line.
fn describe(
    entry: &HistoryEvent,
    outcomes: &Outcomes<'_>,
    payloads: &BTreeMap<Digest, PayloadRecord>,
) -> Option<Line> {
    let line = |event: String, line: Option<u32>, outcome: Option<String>| {
        Some(Line {
            event,
            line,
            outcome,
        })
    };
    let since =
        |end: &HistoryEvent| duration(end.recorded_at.0.saturating_sub(entry.recorded_at.0));
    match &entry.event {
        Event::Lifecycle(Lifecycle::RunStarted(started)) => line(
            format!("run started · input {}", payload(&started.input, payloads)),
            None,
            None,
        ),
        Event::Lifecycle(Lifecycle::RunAborted(reason)) => {
            line(format!("aborted · {reason}"), None, None)
        }
        Event::Command(Command::ScheduleCall {
            seq,
            action,
            args,
            callsite,
        }) => {
            let outcome = match outcomes.calls.get(seq) {
                Some(end) => match &end.event {
                    Event::Completion(Completion::CallCompleted { result, .. }) => format!(
                        "→ {} · {} · event {}",
                        summary(result, payloads),
                        since(end),
                        end.index
                    ),
                    Event::Completion(Completion::CallFailed { error, .. }) => format!(
                        "→ ActionError {}: {} · {} · event {}",
                        error.kind.as_str(),
                        truncate(&error.message, PREVIEW_CHARS),
                        since(end),
                        end.index
                    ),
                    _ => String::new(),
                },
                None => "→ no outcome".to_owned(),
            };
            line(
                format!("{seq} {action}({})", payload(args, payloads)),
                callsite.map(|callsite| callsite.line),
                Some(outcome),
            )
        }
        Event::Command(Command::StartTimer {
            seq,
            duration_ms,
            callsite,
            ..
        }) => {
            let mut event = format!("timer {seq} {}", duration(*duration_ms));
            if let Some(signal) = outcomes.timeouts.get(seq) {
                let _ = write!(event, " (timeout of signal {signal:?})");
            }
            let outcome = match outcomes.timers.get(seq).map(|end| &end.event) {
                Some(Event::Completion(_)) => "→ fired",
                Some(_) => "→ cancelled",
                None if outcomes.ended => "→ dropped with the run",
                None => "→ pending",
            };
            line(event, callsite.map(|c| c.line), Some(outcome.to_owned()))
        }
        Event::Annotation(Annotation::SignalAwaited { name, callsite, .. }) => {
            let outcome = match outcomes.waits.get(&entry.index) {
                Some(end) => match &end.event {
                    Event::Completion(Completion::SignalReceived { .. }) => {
                        format!("→ received after {} · event {}", since(end), end.index)
                    }
                    _ => format!("→ timed out after {}", since(end)),
                },
                None if outcomes.ended => "→ ended with the run".to_owned(),
                None => "→ waiting".to_owned(),
            };
            line(
                format!("waiting for signal {name:?}"),
                callsite.map(|c| c.line),
                Some(outcome),
            )
        }
        Event::Completion(Completion::SignalReceived {
            name,
            payload: value,
        }) => line(
            format!("signal {name:?} ← {}", payload(value, payloads)),
            None,
            None,
        ),
        Event::Completion(Completion::CancelRequested { reason }) => {
            line(format!("cancellation requested: {reason}"), None, None)
        }
        Event::Annotation(Annotation::Log {
            level,
            message,
            callsite,
        }) => line(
            format!(
                "log {} {}",
                level.as_str(),
                truncate(message, MESSAGE_CHARS)
            ),
            callsite.map(|c| c.line),
            None,
        ),
        Event::Command(Command::CompleteRun { output }) => line(
            format!("completed → {}", payload(output, payloads)),
            None,
            None,
        ),
        Event::Command(Command::FailRun { error }) => {
            let location = error.stack.as_deref().and_then(code_line);
            line(
                format!(
                    "failed · {}: {}",
                    error.name,
                    truncate(&error.message, MESSAGE_CHARS)
                ),
                location,
                None,
            )
        }
        Event::Completion(
            Completion::CallCompleted { .. }
            | Completion::CallFailed { .. }
            | Completion::TimerFired { .. },
        )
        | Event::Command(Command::CancelTimer { .. }) => None,
    }
}

/// The first `workflow.ts:<line>` of a stack.
fn code_line(stack: &str) -> Option<u32> {
    let after = crate::failures::after_code_file(stack)?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// A payload, truncated: its value when held, its preview and size otherwise.
fn payload(payload: &Payload, rows: &BTreeMap<Digest, PayloadRecord>) -> String {
    match payload {
        Payload::Value(_) => payload.preview(PREVIEW_CHARS),
        Payload::Digest(digest) => match rows.get(digest) {
            Some(row) => format!(
                "{} ({})",
                truncate(&row.preview, PREVIEW_CHARS),
                bytes(row.size)
            ),
            None => payload.preview(PREVIEW_CHARS),
        },
    }
}

/// A result in a few words: the number of items of an array, or its preview; with its size.
fn summary(result: &Payload, rows: &BTreeMap<Digest, PayloadRecord>) -> String {
    match result {
        Payload::Value(JsonValue::Array(items)) => format!(
            "{} items · {}",
            items.len(),
            bytes(canonical_json(&JsonValue::Array(items.clone())).len() as u64)
        ),
        Payload::Value(value) => format!(
            "{} · {}",
            result.preview(PREVIEW_CHARS),
            bytes(canonical_json(value).len() as u64)
        ),
        Payload::Digest(_) => payload(result, rows),
    }
}

fn bytes(size: u64) -> String {
    match size {
        0..1_000 => format!("{size} B"),
        1_000..1_000_000 => format!("{:.1} KB", size as f64 / 1_000.0),
        _ => format!("{:.1} MB", size as f64 / 1_000_000.0),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let text = text.replace('\n', "⏎");
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// `+0.3s`, `+5m12s`, `+3h05m`, `+2d3h`: logical time since the start.
fn elapsed(at: Timestamp, started_at: Timestamp) -> String {
    format!("+{}", duration(at.0.saturating_sub(started_at.0)))
}

/// A duration in milliseconds, in the largest units that read well.
fn duration(ms: u64) -> String {
    const S: u64 = 1_000;
    const M: u64 = 60 * S;
    const H: u64 = 60 * M;
    const D: u64 = 24 * H;
    match ms {
        0..M => format!("{}.{}s", ms / S, (ms % S) / 100),
        M..H => format!("{}m{}s", ms / M, (ms % M) / S),
        H..D => format!("{}h{:02}m", ms / H, (ms % H) / M),
        _ if ms.is_multiple_of(D) => format!("{}d", ms / D),
        _ => format!("{}d {}h", ms / D, (ms % D) / H),
    }
}

/// A timestamp in RFC 3339, UTC, to the second.
fn rfc3339(at: Timestamp) -> String {
    let secs = at.as_millis() / 1_000;
    let days = i64::try_from(secs / 86_400).unwrap_or(i64::MAX);
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3_600,
        (rem % 3_600) / 60,
        rem % 60
    )
}

/// The proleptic Gregorian date of a number of days since 1970-01-01 (Howard Hinnant's
/// algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_times_and_durations() {
        assert_eq!(
            rfc3339(Timestamp(1_767_225_600_000)),
            "2026-01-01T00:00:00Z"
        );
        assert_eq!(rfc3339(Timestamp(951_782_400_000)), "2000-02-29T00:00:00Z");
        assert_eq!(duration(310), "0.3s");
        assert_eq!(duration(312_000), "5m12s");
        assert_eq!(duration(3 * 3_600_000 + 5 * 60_000), "3h05m");
        assert_eq!(duration(3 * 86_400_000), "3d");
        assert_eq!(duration(2 * 86_400_000 + 23 * 3_600_000), "2d 23h");
        assert_eq!(bytes(12_400), "12.4 KB");
        assert_eq!(
            code_line("Error: x\n    at main (file:///workflow.ts:12:5)"),
            Some(12)
        );
    }
}
