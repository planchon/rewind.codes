//! What the agent tools say to the model: a port of the SDK's
//! `sdk/typescript/src/agent-tools/render.ts` and `bound.ts`, on the same DTOs, so that a model
//! reads in the evals what it reads through the SDK. Plain text, short, bounded, pointing at
//! the next useful call.

use ptc_api::common::{Callsite, Payload, RunStatus};
use ptc_api::runs::{FailedCall, Run, RunError, RunSummary, Waiting};
use ptc_api::scripts::{CheckResult, Diagnostic, Severity, TestResult};
use ptc_api::tasks::{ParkedReason, TaskState};
use ptc_api::toolsets::ToolMatch;
use serde_json::Value;

/// What `run_script` returns of a run's output at most: 256 KiB, the bound of the API's inline
/// values.
pub const OUTPUT_LIMIT: usize = 256 * 1024;

/// The bounds of each part of a tool result, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// A run's output, and each page of `get_run`.
    pub output: usize,
    /// A run's LLM trace.
    pub trace: usize,
    /// A value shown in passing: a failed call's arguments, a test's output.
    pub value: usize,
    /// One log line, a stack.
    pub line: usize,
}

impl Limits {
    pub fn for_output(output: usize) -> Self {
        Self {
            output,
            trace: (output / 2).max(4096),
            value: (output / 64).max(1024),
            line: 2048,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::for_output(OUTPUT_LIMIT)
    }
}

// -------------------------------------------------------------------------------------------
// Bounds (bound.ts)

/// A window of a text, in UTF-8 bytes, cut on character boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slice<'a> {
    pub text: &'a str,
    /// The byte offset of the window's start.
    pub start: usize,
    /// The byte offset just past the window's end: where the next window starts.
    pub end: usize,
    /// The size of the whole text, in bytes.
    pub total: usize,
}

/// At most `limit` bytes of `text` from the byte offset `start`, without cutting a character.
pub fn slice_utf8(text: &str, start: usize, limit: usize) -> Slice<'_> {
    let total = text.len();
    let mut from = start.min(total);
    while from < total && !text.is_char_boundary(from) {
        from += 1;
    }
    let mut to = total.min(from.saturating_add(limit));
    while to < total && to > from && !text.is_char_boundary(to) {
        to -= 1;
    }
    Slice {
        text: &text[from..to],
        start: from,
        end: to,
        total,
    }
}

/// A text cut to at most `limit` bytes by leaving out its middle: a quarter of the room for
/// its start, the rest for its end, where the most recent lines of a trace are.
pub fn truncate_middle(text: &str, limit: usize) -> String {
    let total = text.len();
    if total <= limit {
        return text.to_owned();
    }
    let marker = |left: usize| format!("\n[… {left} bytes left out …]\n");
    let room = limit.saturating_sub(marker(total).len());
    let head = slice_utf8(text, 0, room / 4);
    let tail_room = room - (head.end - head.start);
    let tail = slice_utf8(text, total.saturating_sub(tail_room), room);
    format!(
        "{}{}{}",
        head.text,
        marker(tail.start - head.end),
        tail.text
    )
}

/// A text cut to at most `limit` bytes, saying how much was left out.
pub fn truncate_end(text: &str, limit: usize) -> String {
    let slice = slice_utf8(text, 0, limit);
    if slice.end == slice.total {
        text.to_owned()
    } else {
        format!("{}[… {} more bytes]", slice.text, slice.total - slice.end)
    }
}

// -------------------------------------------------------------------------------------------
// Runs

/// The parts of a run's report that take a request of their own.
#[derive(Debug, Clone, Default)]
pub struct RunExtras {
    /// The output as compact JSON, when the run completed.
    pub output: Option<String>,
    /// The LLM trace.
    pub trace: Option<String>,
}

/// A run's state for the model: its outcome, or what it waits for.
pub fn describe_run(run: &Run, extras: &RunExtras, limits: &Limits) -> String {
    let mut lines = vec![headline(run)];
    match run.status {
        RunStatus::Completed => {
            if let Some(output) = &extras.output {
                lines.extend(output_section(&run.id, output, limits.output));
            }
        }
        RunStatus::Failed | RunStatus::Cancelled => {
            if let Some(error) = &run.error {
                lines.extend(error_section(error, limits));
            }
        }
        RunStatus::Active => lines.extend(waiting_section(run)),
        RunStatus::Aborted | RunStatus::Pending => {}
    }
    if let Some(trace) = &extras.trace {
        lines.push(
            if run.status == RunStatus::Failed {
                "Trace up to the failure:"
            } else {
                "Trace:"
            }
            .to_owned(),
        );
        lines.push(truncate_middle(trace.trim_end(), limits.trace));
    } else if !run.logs.is_empty() {
        lines.push("Last log lines:".to_owned());
        for log in &run.logs {
            lines.push(format!(
                "  [{}]{} {}",
                log_level(log.level),
                at(log.callsite.as_ref()),
                truncate_end(&log.message, limits.line)
            ));
        }
    }
    if matches!(run.status, RunStatus::Active | RunStatus::Pending) {
        lines.push(format!(
            "Follow it with get_run({}), send it a signal with signal_run, or cancel it with cancel_run.",
            args(&[("run_id", Value::from(run.id.clone()))])
        ));
    }
    lines.join("\n")
}

fn log_level(level: ptc_api::common::LogLevel) -> &'static str {
    match level {
        ptc_api::common::LogLevel::Debug => "debug",
        ptc_api::common::LogLevel::Info => "info",
        ptc_api::common::LogLevel::Warn => "warn",
        ptc_api::common::LogLevel::Error => "error",
    }
}

fn headline(run: &Run) -> String {
    let fork = run
        .parent
        .as_ref()
        .map(|parent| format!(" (a fork of run {})", parent.run))
        .unwrap_or_default();
    let name = format!("Run {}{fork}", run.id);
    match run.status {
        RunStatus::Completed => format!("{name} completed."),
        RunStatus::Failed => format!("{name} failed."),
        RunStatus::Cancelled => format!("{name} was cancelled."),
        RunStatus::Aborted => match &run.abort {
            Some(abort) => format!(
                "{name} was aborted by the host ({}): {}",
                abort_kind(abort.kind),
                abort.message
            ),
            None => format!("{name} was aborted by the host."),
        },
        RunStatus::Pending => format!("{name} is pending: it has not started executing yet."),
        RunStatus::Active => {
            if let Some(quarantine) = &run.quarantine {
                let cause = match quarantine.cause {
                    ptc_api::runs::QuarantineCause::NonDeterminism => "non_determinism",
                    ptc_api::runs::QuarantineCause::RepeatedFailures => "repeated_failures",
                };
                return format!(
                    "{name} is quarantined ({cause}): {}. It waits for the platform to be fixed; it is not a fault of the script.",
                    quarantine.reason
                );
            }
            if is_suspended(run) {
                format!("{name} is suspended, waiting for:")
            } else {
                format!("{name} is still running.")
            }
        }
    }
}

fn abort_kind(kind: ptc_api::common::AbortKind) -> &'static str {
    match kind {
        ptc_api::common::AbortKind::LimitExceeded => "limit_exceeded",
        ptc_api::common::AbortKind::NonDeterminism => "non_determinism",
        ptc_api::common::AbortKind::Cancelled => "cancelled",
        ptc_api::common::AbortKind::InvalidScript => "invalid_script",
        ptc_api::common::AbortKind::Internal => "internal",
    }
}

/// Waiting for something other than tool calls in flight: a signal, a timer, the harness.
pub fn is_suspended(run: &Run) -> bool {
    run.waiting_for.iter().any(|waiting| match waiting {
        Waiting::ToolCall { state, .. } => !matches!(
            state,
            TaskState::Ready | TaskState::Claimed | TaskState::Done
        ),
        Waiting::Timer { .. } | Waiting::Signal { .. } => true,
    })
}

fn waiting_section(run: &Run) -> Vec<String> {
    if run.waiting_for.is_empty() || run.quarantine.is_some() {
        return Vec::new();
    }
    let mut lines = if is_suspended(run) {
        Vec::new()
    } else {
        vec!["Waiting for:".to_owned()]
    };
    for item in &run.waiting_for {
        lines.push(format!("- {}", describe_waiting(item)));
    }
    lines
}

fn describe_waiting(waiting: &Waiting) -> String {
    match waiting {
        Waiting::Signal {
            name,
            timeout_at,
            callsite,
        } => {
            let timeout = timeout_at
                .map(|at| format!(", until {at} (then it gets undefined)"))
                .unwrap_or_default();
            format!(
                "the signal {}{}{timeout}: send it with signal_run",
                quoted(name),
                at(callsite.as_ref())
            )
        }
        Waiting::Timer {
            fire_at, callsite, ..
        } => format!("a timer{}, firing at {fire_at}", at(callsite.as_ref())),
        Waiting::ToolCall {
            seq,
            tool,
            state,
            parked,
            attempt,
            callsite,
            ..
        } => {
            let call = format!("the call {seq} to {tool}{}", at(callsite.as_ref()));
            let attempt = if *attempt > 1 {
                format!(" (attempt {attempt})")
            } else {
                String::new()
            };
            match state {
                TaskState::Approval => format!("{call}: it waits for the harness's approval"),
                TaskState::Parked => {
                    if *parked == Some(ParkedReason::CredentialsExpired) {
                        format!(
                            "{call}: parked, the server refused the credentials; the harness must renew them"
                        )
                    } else {
                        format!(
                            "{call}: parked, the tool changed since the toolset was pinned; the harness decides whether it goes on"
                        )
                    }
                }
                TaskState::Deferred => format!("{call}: waiting for its answer{attempt}"),
                TaskState::Claimed => format!("{call}: in progress{attempt}"),
                TaskState::Ready => format!("{call}: queued{attempt}"),
                TaskState::Done => format!("{call}: answered, about to be delivered"),
            }
        }
    }
}

fn output_section(run_id: &str, output: &str, limit: usize) -> Vec<String> {
    let slice = slice_utf8(output, 0, limit);
    if slice.end == slice.total {
        return vec!["Output:".to_owned(), output.to_owned()];
    }
    vec![
        format!(
            "Output ({} bytes; the first {} are shown, read the rest with get_run({})):",
            slice.total,
            slice.end,
            args(&[
                ("run_id", Value::from(run_id)),
                ("offset", Value::from(slice.end)),
            ])
        ),
        slice.text.to_owned(),
    ]
}

fn error_section(error: &RunError, limits: &Limits) -> Vec<String> {
    let mut lines = vec![format!(
        "{}: {}{}",
        error.name,
        error.message,
        at(error.location.as_ref())
    )];
    if let Some(frame) = &error.frame {
        lines.push(frame.trim_end().to_owned());
    }
    if let Some(call) = &error.failed_call {
        lines.extend(failed_call_section(call, limits));
    }
    if let Some(stack) = &error.stack {
        lines.push("Stack:".to_owned());
        lines.push(truncate_end(stack.trim_end(), limits.line));
    }
    lines
}

fn failed_call_section(call: &FailedCall, limits: &Limits) -> Vec<String> {
    let data = call
        .error
        .data
        .as_ref()
        .map(|data| format!(" {}", truncate_end(&json(data), limits.value)))
        .unwrap_or_default();
    vec![
        format!(
            "The call that failed: {} (call {}{}) with {}",
            call.tool,
            call.seq,
            at(call.callsite.as_ref()),
            payload_text(&call.args, limits.value)
        ),
        format!(
            "  {}: {}{data}",
            failure_kind(call.error.kind),
            call.error.message
        ),
    ]
}

fn failure_kind(kind: ptc_api::common::ToolFailureKind) -> &'static str {
    match kind {
        ptc_api::common::ToolFailureKind::Error => "error",
        ptc_api::common::ToolFailureKind::Unavailable => "unavailable",
        ptc_api::common::ToolFailureKind::Timeout => "timeout",
        ptc_api::common::ToolFailureKind::OutcomeUnknown => "outcome_unknown",
    }
}

/// A page of a long text: the run's output, or an entry.
pub fn describe_page(
    title: &str,
    text: &str,
    offset: usize,
    limit: usize,
    next: impl Fn(usize) -> String,
) -> String {
    let slice = slice_utf8(text, offset, limit);
    if slice.start == 0 && slice.end == slice.total {
        return format!("{title}:\n{text}");
    }
    let mut lines = vec![
        format!(
            "{title}, bytes {} to {} of {}:",
            slice.start, slice.end, slice.total
        ),
        slice.text.to_owned(),
    ];
    if slice.end < slice.total {
        lines.push(format!("Read the rest with {}.", next(slice.end)));
    }
    lines.join("\n")
}

/// One journal entry in full.
pub fn entry_text(entry: &ptc_api::events::Entry) -> String {
    let event = serde_json::to_value(&entry.event).unwrap_or(Value::Null);
    format!(
        "{{\"idx\":{},\"recordedAt\":{},\"event\":{}}}",
        entry.idx,
        json(&Value::from(entry.recorded_at.to_string())),
        json(&event)
    )
}

// -------------------------------------------------------------------------------------------
// Checks, tests, lists, searches

/// The diagnostics of a check, for the model.
pub fn describe_check(result: &CheckResult) -> String {
    let errors = result
        .diagnostics
        .iter()
        .filter(|d| d.severity == Severity::Error)
        .count();
    let warnings = result.diagnostics.len() - errors;
    let counts: Vec<String> = [plural(errors, "error"), plural(warnings, "warning")]
        .into_iter()
        .filter(|part| !part.starts_with("0 "))
        .collect();
    if result.ok {
        return if warnings == 0 {
            "The script type-checks.".to_owned()
        } else {
            format!(
                "The script type-checks, with {}:\n{}",
                counts.join(" and "),
                diagnostics_text(&result.diagnostics)
            )
        };
    }
    format!(
        "The script does not type-check: {}.\n{}",
        counts.join(" and "),
        diagnostics_text(&result.diagnostics)
    )
}

/// A script refused before any run exists.
pub fn describe_refusal(detail: &str, diagnostics: &[Diagnostic]) -> String {
    let text = format!("The script was refused before anything ran: {detail}");
    if diagnostics.is_empty() {
        text
    } else {
        format!("{text}\n{}", diagnostics_text(diagnostics))
    }
}

pub fn diagnostics_text(diagnostics: &[Diagnostic]) -> String {
    diagnostics
        .iter()
        .map(|d| {
            let location = d
                .location
                .map(|l| format!("script.ts:{}:{} ", l.line, l.column))
                .unwrap_or_default();
            let mut lines = vec![format!(
                "{location}{} {}: {}",
                d.severity.as_str(),
                d.code,
                d.message
            )];
            if let Some(frame) = &d.frame {
                lines.push(frame.trim_end().to_owned());
            }
            if let Some(hint) = &d.hint {
                lines.push(format!("hint: {hint}"));
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The outcome of a scenario.
pub fn describe_test(result: &TestResult, limits: &Limits) -> String {
    let mut lines = Vec::new();
    if result.passed {
        lines.push("The test passed.".to_owned());
    } else if result.failures.is_empty() {
        lines.push("The test failed.".to_owned());
    } else {
        lines.push(format!(
            "The test failed: {} do not hold:",
            plural(result.failures.len(), "expectation")
        ));
        lines.extend(result.failures.iter().map(|failure| format!("- {failure}")));
    }
    lines.push(format!("The run {}.", status_phrase(result.status)));
    if let Some(output) = &result.output
        && result.status == RunStatus::Completed
    {
        lines.push(format!(
            "Output: {}",
            truncate_end(&json(output), limits.output)
        ));
    }
    if let Some(error) = &result.error {
        lines.push(format!("{}: {}", error.name, error.message));
    }
    if let Some(abort) = &result.abort {
        lines.push(format!(
            "Aborted ({}): {}",
            abort_kind(abort.kind),
            abort.message
        ));
    }
    if result.calls.is_empty() {
        lines.push("No tool was called.".to_owned());
    } else {
        let calls: Vec<String> = result
            .calls
            .iter()
            .map(|(tool, count)| format!("{tool} ×{count}"))
            .collect();
        lines.push(format!("Calls: {}", calls.join(", ")));
    }
    if !result.trace.trim().is_empty() {
        lines.push("Trace:".to_owned());
        lines.push(truncate_middle(result.trace.trim_end(), limits.trace));
    }
    lines.join("\n")
}

/// A list of runs.
pub fn describe_runs(runs: &[RunSummary], more: bool, status: Option<RunStatus>) -> String {
    let which = status
        .map(|status| format!(" with the status {}", status.as_str()))
        .unwrap_or_default();
    if runs.is_empty() {
        return format!("No runs{which}.");
    }
    let mut lines = vec![format!(
        "{}{which}, most recent first:",
        plural(runs.len(), "run")
    )];
    for run in runs {
        let error = run
            .error
            .as_ref()
            .map(|error| format!(" ({error})"))
            .unwrap_or_default();
        let mut parts = vec![format!("{}: {}{error}", run.id, run.status.as_str())];
        if run.quarantined {
            parts.push("quarantined".to_owned());
        }
        parts.push(format!("started {}", run.started_at));
        parts.push(plural(
            usize::try_from(run.tool_calls).unwrap_or(0),
            "tool call",
        ));
        if run.errors > 0 {
            parts.push(format!("{} failed", run.errors));
        }
        if let Some(parent) = &run.parent {
            parts.push(format!("a fork of {parent}"));
        }
        lines.push(format!("- {}", parts.join(", ")));
    }
    if more {
        lines.push("More runs exist: narrow the list with a status.".to_owned());
    }
    lines.join("\n")
}

/// The tools matching a search.
pub fn describe_matches(query: &str, matches: &[ToolMatch]) -> String {
    if matches.is_empty() {
        return format!("No tool matches {}.", quoted(query));
    }
    let mut parts = vec![format!(
        "{} {} {}:",
        plural(matches.len(), "tool"),
        if matches.len() == 1 {
            "matches"
        } else {
            "match"
        },
        quoted(query)
    )];
    parts.extend(matches.iter().map(|m| m.dts.trim_end().to_owned()));
    parts.join("\n\n")
}

fn status_phrase(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "completed",
        RunStatus::Failed => "failed",
        RunStatus::Cancelled => "was cancelled",
        RunStatus::Aborted => "was aborted",
        RunStatus::Pending => "never started",
        RunStatus::Active => "was still waiting at the end of the scenario",
    }
}

fn payload_text(payload: &Payload, limit: usize) -> String {
    match &payload.value {
        Some(value) => truncate_end(&json(value), limit),
        None => format!("{}… ({} bytes)", payload.preview, payload.bytes),
    }
}

fn at(callsite: Option<&Callsite>) -> String {
    callsite
        .map(|callsite| format!(" at script.ts:{}:{}", callsite.line, callsite.column))
        .unwrap_or_default()
}

/// A tool call as the model would write it: `JSON.stringify` of an object literal, its keys
/// in order.
pub fn args(values: &[(&str, Value)]) -> String {
    let fields: Vec<String> = values
        .iter()
        .map(|(key, value)| format!("{}:{}", json(&Value::from(*key)), json(value)))
        .collect();
    format!("{{{}}}", fields.join(","))
}

/// `JSON.stringify(value)`: compact, numbers written as JavaScript writes them. The API's
/// objects reach the SDK with sorted keys (serde_json maps), which `JSON.stringify` keeps, so
/// RFC 8785's canonical form is what the SDK shows.
pub fn json(value: &Value) -> String {
    ptc_core::canonical_json(value)
}

/// `JSON.stringify` of a string.
pub fn quoted(text: &str) -> String {
    json(&Value::from(text))
}

pub fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn slices_never_cut_a_character() {
        let text = "aé€b";
        let slice = slice_utf8(text, 0, 2);
        assert_eq!(slice.text, "a");
        assert_eq!(slice.end, 1);
        let slice = slice_utf8(text, 2, 3);
        assert_eq!(slice.text, "€");
        assert_eq!((slice.start, slice.end, slice.total), (3, 6, 7));
    }

    #[test]
    fn truncation_keeps_the_end_of_a_trace() {
        let text = "x".repeat(10_000);
        let cut = truncate_middle(&text, 1_000);
        assert!(cut.len() <= 1_000, "{}", cut.len());
        assert!(cut.contains("bytes left out"));
        assert_eq!(truncate_end("abcdef", 3), "abc[… 3 more bytes]");
        assert_eq!(truncate_end("abc", 3), "abc");
    }

    #[test]
    fn json_is_written_as_javascript_writes_it() {
        assert_eq!(
            json(&json!({ "b": 1.0, "a": [1.5, "é"] })),
            r#"{"a":[1.5,"é"],"b":1}"#
        );
        assert_eq!(
            args(&[("run_id", json!("r")), ("offset", json!(10))]),
            r#"{"run_id":"r","offset":10}"#
        );
    }

    #[test]
    fn pages_point_at_the_next_one() {
        let text = "0123456789";
        let page = describe_page("Output of run r", text, 0, 4, |next| {
            format!("next({next})")
        });
        assert_eq!(
            page,
            "Output of run r, bytes 0 to 4 of 10:\n0123\nRead the rest with next(4)."
        );
        let last = describe_page("Output of run r", text, 8, 4, |next| {
            format!("next({next})")
        });
        assert_eq!(last, "Output of run r, bytes 8 to 10 of 10:\n89");
        assert_eq!(describe_page("T", "ab", 0, 4, |_| String::new()), "T:\nab");
    }
}
