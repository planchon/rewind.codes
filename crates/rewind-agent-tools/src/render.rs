//! What the agent tools say to the model, on `rewind-api`'s DTOs (the SDK's
//! `sdk/typescript/src/agent-tools/render.ts` and `bound.ts` say the same of runs and tests), and
//! what the MCP set says of the organizations, namespaces and action sets a caller reaches. Plain
//! text, short, bounded, pointing at the next useful call. A workflow is named by its name, else
//! its id, and a version by its number: `triage@v12`.

use std::collections::BTreeSet;

use rewind_api::access::Permission;
use rewind_api::action_sets::ActionMatch;
use rewind_api::calls::{CallState, ParkedReason};
use rewind_api::checks::{CheckResult, Diagnostic, Severity};
use rewind_api::common::{Callsite, Payload, RunStatus};
use rewind_api::common::{ErrorInfo, FailedCall};
use rewind_api::runs::{Run, RunSummary, Waiting};
use rewind_api::testing::{Test, TestProblem, TestResult};
use rewind_api::workflows::{PatchWorkflow, RecordedTests, RunWorkflow, Workflow, WorkflowVersion};
use serde_json::{Value, json};

/// What `run_workflow` returns of a run's output at most: 256 KiB, the bound of the API's inline
/// values.
pub const OUTPUT_LIMIT: usize = 256 * 1024;

/// The bounds of each part of a tool result, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// A run's output, and each page of `get_run`.
    pub output: usize,
    /// A run's transcript.
    pub transcript: usize,
    /// A value shown in passing: a failed call's arguments, a test's output.
    pub value: usize,
    /// One log line, a stack.
    pub line: usize,
    /// A whole result: room for the parts above, and a last bound on any text left unbounded.
    pub total: usize,
}

impl Limits {
    /// The SDK's bounds: a page of output of `output` bytes, and room around it for a transcript.
    pub fn for_output(output: usize) -> Self {
        let transcript = (output / 2).max(4096);
        Self {
            output,
            transcript,
            value: (output / 64).max(1024),
            line: 2048,
            total: output + transcript + 64 * 1024,
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::for_output(OUTPUT_LIMIT)
    }
}

/// What one result of the MCP set holds at most: 80 000 bytes, so at most 80 000 characters
/// (Claude Code takes 25 000 tokens of a result by default).
pub const MCP_RESULT_LIMIT: usize = 80_000;

impl Limits {
    /// Bounds whose whole result is `total` bytes: half for a page of output, a quarter for a
    /// transcript, the rest for the parts around them.
    pub fn for_total(total: usize) -> Self {
        let output = total / 2;
        Self {
            output,
            transcript: total / 4,
            value: (output / 64).max(1024),
            line: 2048,
            total,
        }
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
/// its start, the rest for its end, where the most recent lines of a transcript are.
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
    /// The transcript.
    pub transcript: Option<String>,
}

/// A run's state for the model: its outcome, or what it waits for.
pub fn describe_run(run: &Run, extras: &RunExtras, limits: &Limits) -> String {
    let mut lines = vec![headline(run, limits)];
    match run.status {
        RunStatus::Completed => {
            if let Some(output) = &extras.output {
                lines.extend(output_section(&run.handle, output, limits.output));
            }
        }
        RunStatus::Failed | RunStatus::Cancelled => {
            if let Some(error) = &run.error {
                lines.extend(error_section(error, limits));
            }
        }
        RunStatus::Waiting => lines.extend(waiting_section(run)),
        RunStatus::Aborted | RunStatus::Queued | RunStatus::Running => {}
    }
    if let Some(transcript) = &extras.transcript {
        lines.push(
            if run.status == RunStatus::Failed {
                "Transcript up to the failure:"
            } else {
                "Transcript:"
            }
            .to_owned(),
        );
        lines.push(truncate_middle(transcript.trim_end(), limits.transcript));
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
    if !run.status.is_ended() {
        lines.push(format!(
            "Follow it with get_run({}), send it a signal with signal_run, or cancel it with cancel_run.",
            args(&[("run_id", Value::from(run.handle.clone()))])
        ));
    }
    lines.join("\n")
}

fn log_level(level: rewind_api::common::LogLevel) -> &'static str {
    match level {
        rewind_api::common::LogLevel::Debug => "debug",
        rewind_api::common::LogLevel::Info => "info",
        rewind_api::common::LogLevel::Warn => "warn",
        rewind_api::common::LogLevel::Error => "error",
    }
}

fn headline(run: &Run, limits: &Limits) -> String {
    let about: Vec<String> = [
        run.source
            .as_ref()
            .map(|parent| format!("a fork of run {}", parent.handle)),
        run.workflow
            .as_ref()
            .map(|workflow| format!("a run of the workflow {}", run_workflow_label(workflow))),
        run.started_by.as_ref().map(|by| {
            format!(
                "started by run {}{}",
                by.handle,
                if by.detached { ", detached" } else { "" }
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    let name = if about.is_empty() {
        format!("Run {}", run.handle)
    } else {
        format!("Run {} ({})", run.handle, about.join("; "))
    };
    match run.status {
        RunStatus::Completed => format!("{name} completed."),
        RunStatus::Failed => format!("{name} failed."),
        RunStatus::Cancelled => format!("{name} was cancelled."),
        RunStatus::Aborted => match &run.abort {
            Some(abort) => format!(
                "{name} was aborted by the host ({}): {}",
                abort_kind(abort.kind),
                truncate_end(&abort.message, limits.value)
            ),
            None => format!("{name} was aborted by the host."),
        },
        RunStatus::Queued => format!("{name} is queued: it has not started executing yet."),
        RunStatus::Running => format!("{name} is still running."),
        RunStatus::Waiting => {
            if let Some(quarantine) = &run.quarantine {
                let cause = match quarantine.cause {
                    rewind_api::runs::QuarantineCause::NonDeterminism => "non_determinism",
                    rewind_api::runs::QuarantineCause::RepeatedFailures => "repeated_failures",
                };
                return format!(
                    "{name} is quarantined ({cause}): {}. It waits for the platform to be fixed; it is not a fault of the code.",
                    truncate_end(&quarantine.reason, limits.value)
                );
            }
            format!("{name} is waiting for:")
        }
    }
}

fn abort_kind(kind: rewind_api::common::AbortKind) -> &'static str {
    match kind {
        rewind_api::common::AbortKind::LimitExceeded => "limit_exceeded",
        rewind_api::common::AbortKind::NonDeterminism => "non_determinism",
        rewind_api::common::AbortKind::Cancelled => "cancelled",
        rewind_api::common::AbortKind::InvalidCode => "invalid_code",
        rewind_api::common::AbortKind::Internal => "internal",
    }
}

fn waiting_section(run: &Run) -> Vec<String> {
    if run.waiting_for.is_empty() || run.quarantine.is_some() {
        return Vec::new();
    }
    run.waiting_for
        .iter()
        .filter(|item| !matches!(item, Waiting::Quarantined { .. }))
        .map(|item| format!("- {}", describe_waiting(item)))
        .collect()
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
        Waiting::Call {
            seq,
            action,
            state,
            parked,
            delivery,
            callsite,
            ..
        } => {
            let call = format!("the call {seq} to {action}{}", at(callsite.as_ref()));
            let delivery = if *delivery > 1 {
                format!(" (delivery {delivery})")
            } else {
                String::new()
            };
            match state {
                CallState::Approval => format!("{call}: it waits for the harness's approval"),
                CallState::Parked => {
                    if *parked == Some(ParkedReason::CredentialsRefused) {
                        format!(
                            "{call}: parked, the server refused the credentials; the harness must renew them"
                        )
                    } else if *parked == Some(ParkedReason::SecretMissing) {
                        format!(
                            "{call}: parked, the secret the run uses no longer exists; the harness must put it again"
                        )
                    } else {
                        format!(
                            "{call}: parked, the action changed since the action set was pinned; the harness decides whether it goes on"
                        )
                    }
                }
                CallState::Deferred => format!("{call}: waiting for its answer{delivery}"),
                CallState::Claimed => format!("{call}: in progress{delivery}"),
                CallState::Ready => format!("{call}: queued{delivery}"),
                CallState::Done => format!("{call}: answered, about to be delivered"),
            }
        }
        Waiting::Quarantined { .. } => "its release from quarantine".to_owned(),
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

fn error_section(error: &ErrorInfo, limits: &Limits) -> Vec<String> {
    let mut lines = vec![format!(
        "{}: {}{}",
        truncate_end(&error.name, limits.line),
        truncate_end(&error.message, limits.value),
        at(error.location.as_ref())
    )];
    if let Some(frame) = &error.frame {
        lines.push(truncate_end(frame.trim_end(), limits.value));
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
            call.action,
            call.seq,
            at(call.callsite.as_ref()),
            payload_text(&call.args, limits.value)
        ),
        format!(
            "  {}: {}{data}",
            failure_kind(call.error.kind),
            truncate_end(&call.error.message, limits.value)
        ),
    ]
}

fn failure_kind(kind: rewind_api::common::ActionFailureKind) -> &'static str {
    match kind {
        rewind_api::common::ActionFailureKind::Error => "error",
        rewind_api::common::ActionFailureKind::Unavailable => "unavailable",
        rewind_api::common::ActionFailureKind::Timeout => "timeout",
        rewind_api::common::ActionFailureKind::OutcomeUnknown => "outcome_unknown",
    }
}

/// A page of a long text: the run's output, or an event.
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

/// One event in full.
pub fn event_text(entry: &rewind_api::events::HistoryEvent) -> String {
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

/// The diagnostics of a check, for the model, and the workflow that already has the code.
pub fn describe_check(result: &CheckResult, limits: &Limits) -> String {
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
    let mut text = if result.ok {
        if warnings == 0 {
            "The code type-checks.".to_owned()
        } else {
            format!(
                "The code type-checks, with {}:\n{}",
                counts.join(" and "),
                diagnostics_text(&result.diagnostics, limits)
            )
        }
    } else {
        format!(
            "The code does not type-check: {}.\n{}",
            counts.join(" and "),
            diagnostics_text(&result.diagnostics, limits)
        )
    };
    if let Some(workflow) = &result.workflow {
        text.push_str(&format!(
            "\nThis code is already the version v{} of the workflow {}: running it is a run of that workflow.",
            workflow.version,
            workflow_of_run_label(workflow)
        ));
    }
    text
}

/// What `check_workflow` adds to its answer for a code without a comment at its top: the
/// comment a workflow without a name shows as (`archi/04-tools.md`, "Workflow names").
pub const COMMENT_NOTE: &str = "Note: the code does not start with a comment that says what it does, so its workflow shows as its short id until it has a name. Start it with:\n/** <what it does, in a few words>\n *\n * <more, if needed> */\nIts first line is how the workflow shows; the lines after it describe it.";

/// A code refused before any run exists.
pub fn describe_refusal(detail: &str, diagnostics: &[Diagnostic], limits: &Limits) -> String {
    describe_refused(
        "The code was refused before anything ran",
        detail,
        diagnostics,
        limits,
    )
}

/// The diagnostics, in order, as many as the room of a transcript holds.
pub fn diagnostics_text(diagnostics: &[Diagnostic], limits: &Limits) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut bytes = 0;
    for d in diagnostics {
        let location = d
            .location
            .map(|l| format!("workflow.ts:{}:{} ", l.line, l.column))
            .unwrap_or_default();
        let mut lines = vec![format!(
            "{location}{} {}: {}",
            d.severity.as_str(),
            d.code,
            truncate_end(&d.message, limits.value)
        )];
        if let Some(frame) = &d.frame {
            lines.push(truncate_end(frame.trim_end(), limits.value));
        }
        if let Some(hint) = &d.hint {
            lines.push(format!("hint: {}", truncate_end(hint, limits.line)));
        }
        let text = lines.join("\n");
        bytes += text.len() + 2;
        if bytes > limits.transcript && !shown.is_empty() {
            shown.push(format!(
                "… and {}.",
                plural(diagnostics.len() - shown.len(), "more diagnostic")
            ));
            break;
        }
        shown.push(text);
    }
    shown.join("\n\n")
}

/// The outcome of tests: a line per test that passed; for each one that did not, its problem,
/// its failures, how its run ended, its calls and its transcript. The transcripts share the room of one
/// transcript.
pub fn describe_test_results(results: &[TestResult], limits: &Limits) -> String {
    if results.is_empty() {
        return "No test ran.".to_owned();
    }
    let failing: Vec<&TestResult> = results.iter().filter(|result| !result.passed).collect();
    let mut lines = vec![if failing.is_empty() {
        if results.len() == 1 {
            "The test passed.".to_owned()
        } else {
            format!("All {} tests passed.", results.len())
        }
    } else {
        format!(
            "{} of {} did not pass.",
            failing.len(),
            plural(results.len(), "test")
        )
    }];
    let transcribed = failing
        .iter()
        .filter(|result| !result.transcript.trim().is_empty())
        .count();
    let transcript_room = (limits.transcript / transcribed.max(1)).max(limits.line);
    for result in results {
        let name = quoted(&truncate_end(&result.name, limits.line));
        if result.passed {
            lines.push(format!("PASS {name}"));
            continue;
        }
        let problem = result
            .problem
            .map(|problem| format!(" ({}): {}", problem_name(problem), problem_phrase(problem)))
            .unwrap_or_default();
        lines.push(format!("FAIL {name}{problem}"));
        lines.extend(
            result
                .failures
                .iter()
                .map(|failure| format!("  - {}", truncate_end(failure, limits.value))),
        );
        let Some(status) = result.status else {
            continue;
        };
        lines.push(format!("  The run {}.", status_phrase(status)));
        if let Some(output) = &result.output
            && status == RunStatus::Completed
        {
            lines.push(format!(
                "  Output: {}",
                truncate_end(&json(output), limits.value)
            ));
        }
        if let Some(error) = &result.error {
            lines.push(format!(
                "  {}: {}",
                truncate_end(&error.name, limits.line),
                truncate_end(&error.message, limits.value)
            ));
        }
        if let Some(abort) = &result.abort {
            lines.push(format!(
                "  Aborted ({}): {}",
                abort_kind(abort.kind),
                truncate_end(&abort.message, limits.value)
            ));
        }
        if result.calls.is_empty() {
            lines.push("  No action was called.".to_owned());
        } else {
            let calls: Vec<String> = result
                .calls
                .iter()
                .map(|(action, count)| format!("{action} ×{count}"))
                .collect();
            lines.push(format!("  Calls: {}", calls.join(", ")));
        }
        let transcript = result.transcript.trim_end();
        if !transcript.trim().is_empty() {
            lines.push("  Transcript:".to_owned());
            lines.push(truncate_middle(transcript, transcript_room));
        }
    }
    lines.join("\n")
}

fn problem_name(problem: TestProblem) -> &'static str {
    match problem {
        TestProblem::Stale => "stale",
        TestProblem::Incompatible => "incompatible",
        TestProblem::Failed => "failed",
    }
}

fn problem_phrase(problem: TestProblem) -> &'static str {
    match problem {
        TestProblem::Stale => {
            "it names actions, answers or args the action set no longer has; it was not run."
        }
        TestProblem::Incompatible => {
            "the workflow's input or signal schemas refuse its input or its signals; it was not run."
        }
        TestProblem::Failed => "it ran, and expectations do not hold:",
    }
}

/// What `update_workflow`'s gate refused: the outcome of each test, and what the model may do.
/// `version` is a version's refusal; otherwise a change of tests', whose tests ran against
/// `latest`, the workflow's latest version.
pub fn describe_gate_refused(
    detail: &str,
    results: &[TestResult],
    workflow: &str,
    version: bool,
    latest: u32,
    limits: &Limits,
) -> String {
    let advice = if version {
        format!(
            "Every test must pass: there is no way around it. If the code is wrong, fix it, and check it with test_workflow({}) before you call update_workflow again. If a test pins a behavior you change on purpose, retire it in `retire` (the behavior is deprecated) or replace it in `replace` (its state or its expected result is out of date), with a reason.",
            args(&[("code", json!("…")), ("workflow", json!(workflow))])
        )
    } else {
        format!(
            "A test joins the suite only once it passes against the latest version, v{latest}. If the test is wrong, fix it. If the workflow must change to pass it, give its new `code` with the test: the version is made with it once the whole suite passes."
        )
    };
    [
        format!(
            "{}: {}",
            if version {
                "No version was made"
            } else {
                "The suite was not changed"
            },
            truncate_end(detail, limits.value)
        ),
        describe_test_results(results, limits),
        advice,
    ]
    .join("\n")
}

/// Tests that refused the fork of a run of a workflow: the outcome of each, and what the model
/// may do. `updates` when `update_workflow` is one of the tools.
pub fn describe_retry_refused(
    run_id: &str,
    detail: &str,
    results: &[TestResult],
    workflow: Option<&str>,
    updates: bool,
    limits: &Limits,
) -> String {
    let suite = workflow.map_or_else(
        || "the suite of its workflow".to_owned(),
        |workflow| format!("the suite of {workflow}"),
    );
    let check = workflow.map_or_else(
        || "test_workflow".to_owned(),
        |workflow| {
            format!(
                "test_workflow({})",
                args(&[("code", json!("…")), ("workflow", json!(workflow))])
            )
        },
    );
    let on_purpose = if updates {
        "If a behavior must change on purpose, make the workflow's next version with update_workflow, retiring or replacing its tests with a reason, and run it."
    } else {
        "If a behavior must change on purpose, the workflow needs a next version that retires or replaces those tests, which these tools do not make: tell the person."
    };
    [
        format!(
            "Run {run_id} was not retried: {}",
            truncate_end(detail, limits.value)
        ),
        describe_test_results(results, limits),
        format!(
            "A fix must keep every behavior {suite} pins: there is no way around it. Fix the code, and check it with {check} before you try again. {on_purpose}"
        ),
    ]
    .join("\n")
}

/// A refusal: what was not done (`what`), why, and the diagnostics of the code.
pub fn describe_refused(
    what: &str,
    detail: &str,
    diagnostics: &[Diagnostic],
    limits: &Limits,
) -> String {
    let text = format!("{what}: {}", truncate_end(detail, limits.value));
    if diagnostics.is_empty() {
        text
    } else {
        format!("{text}\n{}", diagnostics_text(diagnostics, limits))
    }
}

/// How a run's workflow reads in a run's report: `triage@v3, id wf_7k2m9q4x8d1c`, or
/// `wf_7k2m9q4x8d1c@v1` without a name.
pub fn run_workflow_label(workflow: &RunWorkflow) -> String {
    match &workflow.name {
        Some(name) => format!("{name}@v{}, id {}", workflow.version, workflow.id),
        None => format!("{}@v{}", workflow.id, workflow.version),
    }
}

/// A run's workflow without its version: `triage (id wf_7k2m9q4x8d1c)`, or `wf_7k2m9q4x8d1c`.
fn workflow_of_run_label(workflow: &RunWorkflow) -> String {
    match &workflow.name {
        Some(name) => format!("{name} (id {})", workflow.id),
        None => workflow.id.clone(),
    }
}

/// What names a run's version in a call: `triage@v3`, or `wf_7k2m9q4x8d1c@v3` without a name.
pub fn run_workflow_reference(workflow: &RunWorkflow) -> String {
    format!(
        "{}@v{}",
        workflow.name.as_deref().unwrap_or(&workflow.id),
        workflow.version
    )
}

/// What names a workflow in a call: its name, else its id.
pub fn workflow_reference(workflow: &Workflow) -> &str {
    workflow.name.as_deref().unwrap_or(&workflow.id)
}

/// A workflow as the model reads it: `triage (id wf_7k2m9q4x8d1c)`; without a name, its id and
/// how it shows: the first line of its latest version's comment, else its short id.
pub fn workflow_label(workflow: &Workflow) -> String {
    if let Some(name) = &workflow.name {
        return format!("{name} (id {})", workflow.id);
    }
    let shown = latest_version(workflow).comment.as_ref().map_or_else(
        || workflow.short_id.clone(),
        |comment| quoted(&comment.title),
    );
    format!("{} (no name; it shows as {shown})", workflow.id)
}

/// Its latest version the caller sees.
fn latest_version(workflow: &Workflow) -> &WorkflowVersion {
    workflow.versions.first().unwrap_or(&workflow.version)
}

/// What `update_workflow` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Updated {
    /// Its name or its description only.
    Patched,
    /// A version was asked for: made, or the latest already had that content.
    Version { made: bool },
    /// Its suite changed, without a version.
    Tests,
}

/// What a merge patch changed: `named it triage and removed its description`.
pub fn describe_patch(patch: &PatchWorkflow) -> Option<String> {
    let parts: Vec<String> = [
        patch.name.as_ref().map(|name| match name {
            Some(name) => format!("named it {name}"),
            None => "removed its name".to_owned(),
        }),
        patch
            .description
            .as_ref()
            .map(|description| match description {
                Some(_) => "gave it a new description".to_owned(),
                None => "removed its description".to_owned(),
            }),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" and "))
}

/// `update_workflow`'s result: the workflow's id, its name, and its version, new or unchanged.
pub fn describe_update(
    workflow: &Workflow,
    patch: &PatchWorkflow,
    updated: Updated,
    limits: &Limits,
) -> String {
    let label = workflow_label(workflow);
    let version = &workflow.version;
    let mut lines = Vec::new();
    if let Some(patched) = describe_patch(patch) {
        lines.push(format!("Updated the workflow {label}: {patched}."));
    }
    match updated {
        Updated::Patched => lines.push(format!(
            "Its version stays v{}.",
            latest_version(workflow).version
        )),
        Updated::Version { made: true } => lines.push(format!(
            "Made the version v{} (digest {}) of the workflow {label}: its suite, {}, passed against it.",
            version.version,
            short_digest(&version.digest),
            plural(version.tests.len(), "test")
        )),
        Updated::Version { made: false } => lines.push(format!(
            "The workflow {label} already has this content as its version v{}: no version was made.",
            version.version
        )),
        Updated::Tests => lines.push(format!(
            "Changed the suite of the workflow {label}: it holds {}, each passing against its version v{}, which stays its latest. Its next version keeps them.",
            plural(
                workflow
                    .suite
                    .iter()
                    .filter(|entry| entry.retired_at.is_none())
                    .count(),
                "test"
            ),
            latest_version(workflow).version
        )),
    }
    if let Some(declarations) = &version.declarations
        && !declarations.prompt.trim().is_empty()
    {
        lines.push(format!(
            "Code starts it from \"rewind:workflows\" once an action set pins it: {}",
            truncate_end(declarations.prompt.trim(), limits.value)
        ));
    }
    lines.push(format!(
        "Run it with run_workflow({}).",
        args(&[("workflow", json!(workflow_reference(workflow)))])
    ));
    lines.join("\n")
}

/// The first 12 digits of a digest.
fn short_digest(digest: &str) -> &str {
    digest.get(..12).unwrap_or(digest)
}

/// What `get_workflow` reads: the workflow, and each test of its suite.
#[derive(Debug, Clone)]
pub struct WorkflowDetails {
    pub workflow: Workflow,
    /// The tests of its suite, in its order.
    pub tests: Vec<ReadTest>,
    /// `update_workflow` is one of the tools.
    pub updates: bool,
}

/// A test of a suite as `get_workflow` reads it: the test, or why it cannot be read.
#[derive(Debug, Clone)]
pub struct ReadTest {
    pub name: String,
    pub digest: String,
    pub test: Option<Test>,
    pub unreadable: Option<String>,
}

/// The most versions `get_workflow` lists.
const LISTED_VERSIONS: usize = 20;

/// A workflow, for the model to repair or extend it: who it is, its version's code and
/// interface, its suite, the tests that left it, and its versions.
pub fn describe_workflow_details(details: &WorkflowDetails, limits: &Limits) -> String {
    let workflow = &details.workflow;
    let version = &workflow.version;
    let versions = &workflow.versions;
    let scope = match &workflow.namespace {
        Some(namespace) => format!("the namespace {}", namespace.as_str()),
        None => match &workflow.promoted_at {
            Some(at) => format!("the organization (promoted {at})"),
            None => "the organization".to_owned(),
        },
    };
    let mut lines = vec![format!(
        "The workflow {}, of {scope}.",
        workflow_label(workflow)
    )];
    if let Some(source) = &workflow.source {
        lines.push(format!(
            "It is a fork of the organization's workflow {}, whose version v{} it last took.",
            source.workflow, source.version
        ));
    }
    let description = workflow.description.as_deref().or_else(|| {
        latest_version(workflow)
            .comment
            .as_ref()
            .and_then(|comment| comment.description.as_deref())
    });
    if let Some(description) = description {
        lines.push(format!(
            "Description: {}",
            truncate_end(description, limits.value)
        ));
    }
    let from = version
        .base
        .as_ref()
        .map(|base| format!(", from {}", version_ref(base, versions)))
        .unwrap_or_default();
    lines.push(format!(
        "Its version v{} of {} (digest {}), made {}{from}.",
        version.version,
        plural(versions.len().max(1), "version"),
        short_digest(&version.digest),
        version.created_at
    ));
    lines.push(format!("Action set: {}", version.action_set));
    if version.input.is_none() && version.output.is_none() && version.signals.is_none() {
        lines.push(
            "Interface: none. It takes any input, returns any output and accepts any signal name."
                .to_owned(),
        );
    } else {
        let schema = |schema: &Option<Value>| {
            truncate_end(
                &json(schema.as_ref().unwrap_or(&Value::Bool(true))),
                limits.value,
            )
        };
        lines.push(format!("Input schema: {}", schema(&version.input)));
        lines.push(format!("Output schema: {}", schema(&version.output)));
        match version
            .signals
            .as_ref()
            .filter(|signals| !signals.is_empty())
        {
            None => lines.push("Signals: none.".to_owned()),
            Some(signals) => {
                lines.push("Signals:".to_owned());
                for (name, schema) in signals {
                    lines.push(format!(
                        "- {}: {}",
                        quoted(name),
                        truncate_end(&json(schema), limits.value)
                    ));
                }
            }
        }
    }
    if !version.secrets.is_empty() {
        let secrets: serde_json::Map<String, Value> = version
            .secrets
            .iter()
            .map(|(server, secret)| (server.clone(), Value::from(secret.as_str())))
            .collect();
        lines.push(format!(
            "Secrets its runs use: {}",
            json(&Value::Object(secrets))
        ));
    }
    if let Some(declarations) = &version.declarations
        && !declarations.prompt.trim().is_empty()
    {
        lines.push(format!(
            "Code starts it from \"rewind:workflows\" once an action set pins it: {}",
            truncate_end(declarations.prompt.trim(), limits.value)
        ));
    }
    lines.push("Code:".to_owned());
    lines.push(truncate_middle(workflow.code.trim_end(), limits.transcript));

    if details.tests.is_empty() {
        lines.push("Its suite is empty: it has no test.".to_owned());
    } else {
        let reuse = if details.updates {
            "give a digest in update_workflow's `tests` or `replace` to reuse a test"
        } else {
            "give a digest in test_workflow's `tests` to run a test against a code"
        };
        lines.push(format!(
            "Its suite, {}, which its next version must pass ({reuse}):",
            plural(details.tests.len(), "test")
        ));
        let room = (limits.output / details.tests.len()).max(limits.value);
        for entry in &details.tests {
            let head = format!(
                "Test {} ({})",
                quoted(&truncate_end(&entry.name, limits.line)),
                entry.digest
            );
            let Some(test) = &entry.test else {
                lines.push(format!(
                    "{head}: it cannot be read: {}",
                    truncate_end(
                        entry.unreadable.as_deref().unwrap_or_default(),
                        limits.value
                    )
                ));
                continue;
            };
            let origin = test
                .from
                .as_ref()
                .map(|origin| {
                    format!(
                        ", recorded from run {} of {}",
                        origin.run,
                        version_ref(&origin.version, versions)
                    )
                })
                .unwrap_or_default();
            lines.push(format!("{head}{origin}:"));
            lines.push(truncate_end(&readable_test(test), room));
        }
    }

    let left: Vec<_> = workflow
        .suite
        .iter()
        .filter(|entry| entry.retired_at.is_some())
        .collect();
    if !left.is_empty() {
        lines.push("Tests that left its suite, and why:".to_owned());
        for entry in left {
            lines.push(format!(
                "- {} ({}), {}: {}",
                quoted(&truncate_end(&entry.name, limits.line)),
                short_digest(&entry.digest),
                entry
                    .retired_at
                    .map(|at| format!("left {at}"))
                    .unwrap_or_default(),
                truncate_end(
                    entry.reason.as_deref().unwrap_or("no reason given"),
                    limits.value
                )
            ));
        }
    }

    lines.push("Its versions, newest first:".to_owned());
    for listed in versions.iter().take(LISTED_VERSIONS) {
        let from = listed.base.as_ref().map_or_else(
            || ", the first version".to_owned(),
            |base| format!(", from {}", version_ref(base, versions)),
        );
        let mut changes = Vec::new();
        for replaced in &listed.replaced {
            changes.push(format!(
                "replaced {}",
                quoted(&truncate_end(&replaced.name, limits.line))
            ));
        }
        for retired in &listed.retired {
            changes.push(format!(
                "retired {}",
                quoted(&truncate_end(&retired.name, limits.line))
            ));
        }
        let changes = if changes.is_empty() {
            String::new()
        } else {
            format!("; {}", changes.join(", "))
        };
        lines.push(format!(
            "- v{}, made {}{from}: {}{changes}.",
            listed.version,
            listed.created_at,
            plural(listed.tests.len(), "test")
        ));
    }
    if versions.len() > LISTED_VERSIONS {
        lines.push(format!(
            "… and {} older.",
            plural(versions.len() - LISTED_VERSIONS, "version")
        ));
    }
    lines.join("\n")
}

/// `v3` when the digest is one of `versions`, otherwise its first 12 digits.
fn version_ref(digest: &str, versions: &[WorkflowVersion]) -> String {
    versions
        .iter()
        .find(|candidate| candidate.digest == digest)
        .map_or_else(
            || short_digest(digest).to_owned(),
            |known| format!("v{}", known.version),
        )
}

/// A test as the model reads it, as `JSON.stringify` writes the API's object (its fields in
/// the order the server writes them): its cassette summarized, since a recorded run's calls are
/// long and rarely useful.
fn readable_test(test: &Test) -> String {
    let mut readable = test.clone();
    if let Some(cassette) = &readable.world.cassette
        && !cassette.is_null()
    {
        readable.world.cassette = Some(json!({ "summary": cassette_summary(cassette) }));
    }
    serde_json::to_string(&readable).unwrap_or_else(|_| "null".to_owned())
}

/// `12 calls matched by_args (github.list_issues ×3, github.close_issue ×9), 1 signal
/// (approval)`.
pub fn cassette_summary(cassette: &Value) -> String {
    let calls = cassette["calls"].as_array().cloned().unwrap_or_default();
    let signals = cassette["signals"].as_array().cloned().unwrap_or_default();
    let matching = cassette["matching"]
        .as_str()
        .map(|matching| format!(" matched {matching}"))
        .unwrap_or_default();
    // In the order each action is first called, as a JavaScript `Map` keeps them.
    let mut by_action: Vec<(String, usize)> = Vec::new();
    for call in &calls {
        let action = call["action"].as_str().unwrap_or("?");
        match by_action.iter_mut().find(|(known, _)| known == action) {
            Some((_, count)) => *count += 1,
            None => by_action.push((action.to_owned(), 1)),
        }
    }
    let actions = by_action
        .iter()
        .map(|(action, count)| format!("{action} ×{count}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut seen = BTreeSet::new();
    let names: Vec<&str> = signals
        .iter()
        .map(|signal| signal["name"].as_str().unwrap_or("?"))
        .filter(|name| seen.insert(*name))
        .collect();
    format!(
        "{}{matching}{}, {}{}",
        plural(calls.len(), "recorded call"),
        if actions.is_empty() {
            String::new()
        } else {
            format!(" ({actions})")
        },
        plural(signals.len(), "signal"),
        if names.is_empty() {
            String::new()
        } else {
            format!(" ({})", names.join(", "))
        }
    )
}

/// The tests a recording stored, and the runs it left out. `updates` when `update_workflow` is
/// one of the tools.
pub fn describe_recorded(
    workflow: &str,
    recorded: &RecordedTests,
    updates: bool,
    limits: &Limits,
) -> String {
    let mut lines = Vec::new();
    if recorded.tests.is_empty() {
        lines.push(format!(
            "No test was recorded: no completed or failed run of {workflow} was selected."
        ));
    } else {
        lines.push(format!(
            "Recorded {} from the runs of {workflow}, one per path:",
            plural(recorded.tests.len(), "test")
        ));
        for test in &recorded.tests {
            lines.push(format!(
                "- {} {}, from run {} ({}): {}",
                test.digest,
                quoted(&test.name),
                test.run,
                test.status.as_str(),
                truncate_end(&test.path, limits.value)
            ));
        }
    }
    if !recorded.skipped.is_empty() {
        lines.push(format!(
            "Skipped {}:",
            plural(recorded.skipped.len(), "run")
        ));
        for skipped in &recorded.skipped {
            lines.push(format!(
                "- {}: {}",
                skipped.run,
                truncate_end(&skipped.reason, limits.value)
            ));
        }
    }
    if !recorded.tests.is_empty() {
        lines.push(if updates {
            format!(
                "They join no suite yet: pass their digests in update_workflow's `tests` to add them to the suite of {workflow}, or {{ \"test\": \"<digest>\", \"expect\": {{…}} }} to give a failed run the expectation it should have met."
            )
        } else {
            "They join no suite: pass their digests in test_workflow's `tests` to run them against a code, or { \"test\": \"<digest>\", \"expect\": {…} } to give a failed run the expectation it should have met.".to_owned()
        });
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
        let mut parts = vec![format!("{}: {}{error}", run.handle, run.status.as_str())];
        if run.quarantined {
            parts.push("quarantined".to_owned());
        }
        parts.push(format!("started {}", run.started_at));
        parts.push(plural(usize::try_from(run.calls).unwrap_or(0), "call"));
        if run.errors > 0 {
            parts.push(format!("{} failed", run.errors));
        }
        if let Some(parent) = &run.source {
            parts.push(format!("a fork of {parent}"));
        }
        if let Some(workflow) = &run.workflow {
            parts.push(format!(
                "a run of the workflow {}",
                run_workflow_reference(workflow)
            ));
        }
        if let Some(started_by) = &run.started_by {
            parts.push(format!("started by {started_by}"));
        }
        lines.push(format!("- {}", parts.join(", ")));
    }
    if more {
        lines.push("More runs exist: narrow the list with a status.".to_owned());
    }
    lines.join("\n")
}

/// The actions matching a search.
pub fn describe_matches(query: &str, matches: &[ActionMatch]) -> String {
    if matches.is_empty() {
        return format!("No action matches {}.", quoted(query));
    }
    let mut parts = vec![format!(
        "{} {} {}:",
        plural(matches.len(), "action"),
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
        RunStatus::Queued => "never started",
        RunStatus::Running | RunStatus::Waiting => "was still waiting at the end of the test",
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
        .map(|callsite| format!(" at workflow.ts:{}:{}", callsite.line, callsite.column))
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
    rewind_core::canonical_json(value)
}

/// `JSON.stringify` of a string.
pub fn quoted(text: &str) -> String {
    json(&Value::from(text))
}

pub fn plural(count: usize, noun: &str) -> String {
    format!("{count} {noun}{}", if count == 1 { "" } else { "s" })
}

// -------------------------------------------------------------------------------------------
// What a caller reaches (the MCP set)

/// What the tools of the MCP set do with a permission, for `list_organizations`; `None` for those they
/// never use.
fn permission_phrase(permission: Permission) -> Option<&'static str> {
    Some(match permission {
        Permission::RunsRead => "read runs and workflows and record tests",
        Permission::RunsStart => "check, test, run and retry workflows",
        Permission::RunsControl => "signal and cancel runs",
        Permission::ActionsRead => "read action sets",
        Permission::NamespacesRead => "list namespaces",
        _ => return None,
    })
}

/// An organization the caller reaches, as `list_organizations` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizationView<'a> {
    pub id: &'a str,
    pub name: Option<&'a str>,
    pub permissions: &'a [Permission],
}

/// `list_organizations`: the organizations, with what the tools may do in each.
pub fn describe_orgs(organizations: &[OrganizationView<'_>]) -> String {
    if organizations.is_empty() {
        return "You reach no organization: ask the person to open one to this connection in Rewind's dashboard.".to_owned();
    }
    let mut lines = vec![if organizations.len() == 1 {
        "You reach one organization: the tools act in it without `organization`.".to_owned()
    } else {
        format!(
            "You reach {} organizations: give one as `organization`, by its id or its name.",
            organizations.len()
        )
    }];
    for organization in organizations {
        let mut phrases: Vec<&str> = organization
            .permissions
            .iter()
            .filter_map(|permission| permission_phrase(*permission))
            .collect();
        phrases.dedup();
        let who = match organization.name {
            Some(name) => format!("{name} (id {})", organization.id),
            None => format!("id {}", organization.id),
        };
        lines.push(if phrases.is_empty() {
            format!("- {who}: these tools can do nothing there.")
        } else {
            format!("- {who}: you may {}.", phrases.join(", "))
        });
    }
    lines.join("\n")
}

/// `list_secrets`: the secrets the caller may use, by name, never a value: the
/// organization's with their labels, then the namespace's.
pub fn describe_secrets(
    organization: &[rewind_api::secrets::Secret],
    namespace: Option<(&str, &[rewind_api::secrets::Secret])>,
) -> String {
    let mut lines = Vec::new();
    for secret in organization {
        let labels: Vec<String> = secret
            .labels
            .0
            .iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect();
        let labels = if labels.is_empty() {
            String::new()
        } else {
            format!(" (labels {})", labels.join(", "))
        };
        lines.push(format!(
            "- {}, the organization's{labels}",
            secret.reference
        ));
    }
    if let Some((name, secrets)) = namespace {
        for secret in secrets {
            lines.push(format!(
                "- {}, the end user's in the namespace {name}",
                secret.reference
            ));
        }
    }
    if lines.is_empty() {
        return "No secret you may use exists yet: a person puts one first, the end user's in their namespace or the organization's.".into();
    }
    format!(
        "The secrets you may use, never a value:\n{}\nName one in `secrets`, by the MCP server or API whose calls need it, such as {{\"github\": \"ns/github\"}}.",
        lines.join("\n")
    )
}

/// `list_namespaces`: a page of namespaces, the most active first, each with its display name
/// beside its name.
pub fn describe_namespaces(
    list: &rewind_api::namespaces::NamespaceList,
    prefix: Option<&str>,
) -> String {
    let starting = prefix
        .map(|prefix| format!(" starting with {}", quoted(prefix)))
        .unwrap_or_default();
    if list.namespaces.is_empty() {
        return format!(
            "No namespace{starting}. A namespace is created by the first run that names it: give a new name as `namespace` to run_workflow."
        );
    }
    let mut lines = vec![format!(
        "{} namespace{}{starting}, the most active this month ({}) first:",
        list.total,
        if list.total == 1 { "" } else { "s" },
        list.month
    )];
    for namespace in &list.namespaces {
        let usage = &namespace.usage;
        let mut parts = vec![
            plural(
                usize::try_from(usage.runs_started).unwrap_or(usize::MAX),
                "run",
            ) + " this month",
            format!("{} active", usage.active_runs),
            format!("last active {}", namespace.last_active_at),
        ];
        if namespace.deleting_since.is_some() {
            parts.push("being erased: it takes no run".to_owned());
        }
        let shown = match &namespace.display_name {
            Some(display) => format!(" ({})", quoted(display)),
            None => String::new(),
        };
        lines.push(format!(
            "- {}{shown}: {}",
            namespace.name.as_str(),
            parts.join(", ")
        ));
    }
    if list.next.is_some() {
        lines.push(format!(
            "{} more: narrow the list with `prefix`.",
            u64::try_from(list.namespaces.len())
                .map_or(0, |shown| list.total.saturating_sub(shown))
        ));
    }
    lines.push(
        if list
            .namespaces
            .iter()
            .any(|namespace| namespace.display_name.is_some())
        {
            "Give one's name, not its display name in parentheses, as `namespace` to check_workflow, test_workflow and run_workflow."
        } else {
            "Give one as `namespace` to check_workflow, test_workflow and run_workflow."
        }
        .to_owned(),
    );
    lines.join("\n")
}

/// Action names `list_action_sets` shows per action set; `search_actions` gives the rest.
const LISTED_ACTIONS: usize = 20;

/// `list_action_sets`: each action set's id, servers, actions and workflows.
pub fn describe_action_sets(list: &rewind_api::action_sets::ActionSetList) -> String {
    if list.action_sets.is_empty() {
        return "No action set in this organization: a code can only use the built-in `signalUrl`. An action set is registered with POST /v1/action-sets, outside these tools.".to_owned();
    }
    let mut lines = vec![format!(
        "{}: give one's id as `action_set`.",
        plural(list.action_sets.len(), "action set")
    )];
    for action_set in &list.action_sets {
        lines.push(format!("- {}", action_set.id));
        if !action_set.servers.is_empty() {
            let servers: Vec<String> = action_set
                .servers
                .iter()
                .map(|server| {
                    format!(
                        "{} ({})",
                        server.name,
                        plural(server.actions as usize, "action")
                    )
                })
                .collect();
            lines.push(format!("  servers: {}", servers.join(", ")));
        }
        if !action_set.actions.is_empty() {
            let shown: Vec<&str> = action_set
                .actions
                .iter()
                .take(LISTED_ACTIONS)
                .map(String::as_str)
                .collect();
            let more = action_set.actions.len().saturating_sub(LISTED_ACTIONS);
            lines.push(format!(
                "  actions: {}{}",
                shown.join(", "),
                if more > 0 {
                    format!(", and {more} more: find them with search_actions")
                } else {
                    String::new()
                }
            ));
        }
        if !action_set.workflows.is_empty() {
            lines.push(format!("  workflows: {}", action_set.workflows.join(", ")));
        }
    }
    lines.push(
        "Read the declarations of the actions you need with search_actions before you write code."
            .to_owned(),
    );
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn what_a_caller_reaches_is_listed_with_how_to_name_it() {
        assert_eq!(
            describe_orgs(&[]),
            "You reach no organization: ask the person to open one to this connection in Rewind's dashboard."
        );
        let reader = [
            Permission::RunsRead,
            Permission::ActionsRead,
            Permission::TokensWrite,
        ];
        assert_eq!(
            describe_orgs(&[
                OrganizationView {
                    id: "org_1",
                    name: Some("Acme"),
                    permissions: &reader
                },
                OrganizationView {
                    id: "org_2",
                    name: None,
                    permissions: &[Permission::TokensWrite]
                },
            ]),
            "You reach 2 organizations: give one as `organization`, by its id or its name.\n- Acme (id org_1): you may read runs and workflows and record tests, read action sets.\n- id org_2: these tools can do nothing there."
        );

        let action_sets: rewind_api::action_sets::ActionSetList = serde_json::from_value(json!({
            "actionSets": [{
                "id": "ab".repeat(32),
                "servers": [{ "name": "github", "transport": "http", "actions": 25 }],
                "actions": (0..25).map(|n| format!("github.action_{n}")).collect::<Vec<_>>(),
                "workflows": ["triage"]
            }]
        }))
        .unwrap();
        let text = describe_action_sets(&action_sets);
        assert!(text.starts_with(&format!("1 action set: give one's id as `action_set`.\n- {}\n  servers: github (25 actions)\n  actions: github.action_0, ", "ab".repeat(32))), "{text}");
        assert!(
            text.contains(
                "github.action_19, and 5 more: find them with search_actions\n  workflows: triage\n"
            ),
            "{text}"
        );
        assert!(
            describe_action_sets(&rewind_api::action_sets::ActionSetList {
                action_sets: Vec::new()
            })
            .starts_with("No action set in this organization")
        );
    }

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
    fn truncation_keeps_the_end_of_a_transcript() {
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
    fn every_text_a_code_a_server_or_an_action_wrote_is_bounded() {
        let limits = Limits::for_output(64 * 1024);
        let long = "m".repeat(3_000_000);
        let run: Run = serde_json::from_value(json!({
            "id": "r", "handle": "alice/wf-7k2m9q/r", "namespace": "alice", "status": "failed", "codeDigest": "s", "actionSet": "t", "epoch": 1,
            "mode": "live", "labels": {}, "traceId": "0", "historyLen": 3,
            "createdAt": "2026-11-12T00:00:00Z", "updatedAt": "2026-11-12T00:00:00Z",
            "expiresAt": "2026-11-13T00:00:00Z",
            "error": {
                "name": long, "message": long, "frame": long,
                "failedCall": {
                    "seq": 1, "action": "crm.find", "args": { "value": {}, "digest": "d", "bytes": 2, "preview": "{}" },
                    "error": { "kind": "error", "message": long }
                }
            }
        }))
        .unwrap();
        let text = describe_run(&run, &RunExtras::default(), &limits);
        assert!(
            text.len() < 4 * limits.value + limits.line,
            "{}",
            text.len()
        );
        assert!(text.contains("more bytes]"));

        let test: TestResult = serde_json::from_value(json!({
            "name": long, "passed": false, "problem": "failed", "failures": [long], "status": "failed",
            "error": { "name": long, "message": long }, "calls": {}, "transcript": ""
        }))
        .unwrap();
        assert!(describe_test_results(&[test], &limits).len() < 4 * limits.value + 3 * limits.line);

        let diagnostic = |message: &str| Diagnostic {
            severity: Severity::Error,
            code: "TS2322".into(),
            message: message.into(),
            location: None,
            frame: Some(long.clone()),
            hint: Some(long.clone()),
        };
        let refusal = describe_refusal(&long, &[diagnostic("bad")], &limits);
        assert!(refusal.len() < 3 * limits.value + limits.line + 100);
        // Diagnostics take the room of a transcript, then say how many are left out.
        let many: Vec<Diagnostic> = (0..1000)
            .map(|n| diagnostic(&format!("error {n}")))
            .collect();
        let text = diagnostics_text(&many, &limits);
        assert!(text.len() <= limits.transcript + limits.value + limits.line + 100);
        assert!(
            text.ends_with("more diagnostics."),
            "{}",
            &text[text.len() - 60..]
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

    // The answers of the server, as the tools read them.

    const RUN_ID: &str = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10";
    const VERSION: &str = "4be1c07a9d3e0000000000000000000000000000000000000000000000000000";
    const ID: &str = "wf_7k2m9q4x8d1c";

    fn results(value: Value) -> Vec<TestResult> {
        serde_json::from_value(value).unwrap()
    }

    /// A version of `triage`, with an interface: its number, digest and tests from `overrides`.
    fn version(overrides: Value) -> Value {
        let mut version = json!({
            "version": 3,
            "digest": VERSION,
            "codeDigest": "cd".repeat(32),
            "actionSet": "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4",
            "input": { "type": "object", "properties": { "repo": { "type": "string" } }, "required": ["repo"] },
            "output": { "type": "object", "properties": { "closed": { "type": "integer" } }, "required": ["closed"] },
            "signals": { "approval": { "type": "object", "properties": { "close": { "type": "boolean" } }, "required": ["close"] } },
            "labels": { "conversation": "c-42" },
            "secrets": { "github": "ns/github" },
            "createdAt": "2026-11-12T10:00:00Z",
            "declarations": {
                "dts": "declare module \"rewind:workflows\" {\n  export const triage: Workflow<{ repo: string }, { closed: number }, { approval: { close: boolean } }>;\n}",
                "prompt": "triage.start({ repo: string }) → run · run.signal(\"approval\", { close: boolean }) · run.result() → { closed: number } · Triage the stale issues of a repository, after approval."
            },
            "tests": []
        });
        for (key, value) in overrides.as_object().cloned().unwrap_or_default() {
            version[key] = value;
        }
        version
    }

    /// The workflow `triage` of the namespace user_2fQ81, its versions newest first: the first
    /// one shown.
    fn workflow(overrides: Value, versions: Vec<Value>) -> Workflow {
        let mut workflow = json!({
            "id": ID,
            "shortId": "wf-7k2m9q",
            "name": "triage",
            "description": "Triage the stale issues of a repository, after approval.",
            "namespace": "user_2fQ81",
            "labels": { "conversation": "c-42" },
            "createdAt": "2026-11-10T10:00:00Z",
            "version": versions[0],
            "code": "export default async () => ({ closed: 0 });\n",
            "versions": versions,
            "suite": []
        });
        for (key, value) in overrides.as_object().cloned().unwrap_or_default() {
            workflow[key] = value;
        }
        serde_json::from_value(workflow).unwrap()
    }

    #[test]
    fn test_results_say_why_each_test_did_not_pass() {
        let limits = Limits::default();
        let text = describe_test_results(
            &results(json!([
                {
                    "name": "closes the stale issues once approved", "passed": true, "status": "completed",
                    "output": { "closed": 1 }, "calls": { "github.close_issue": 1 }, "failures": [],
                    "transcript": "#0 run_started (a passing test's transcript is not shown)"
                },
                {
                    "name": format!("recorded {RUN_ID}"), "passed": false, "problem": "failed", "status": "completed",
                    "output": { "closed": 2 }, "calls": { "github.close_issue": 2, "github.list_issues": 1 },
                    "failures": ["the output's closed is 2, expected 3", "github.delete_repo has no recorded response"],
                    "transcript": "#0 run_started\n#1 call github.list_issues"
                },
                {
                    "name": "posts to slack", "passed": false, "problem": "stale", "calls": {},
                    "failures": ["slack.post_message is not one of the actions"], "transcript": ""
                },
                {
                    "name": "refuses a bad repo", "passed": false, "problem": "incompatible", "calls": {},
                    "failures": ["the input does not match the input schema of triage: repo must be a string"], "transcript": ""
                },
                {
                    "name": "fails when GitHub is down", "passed": false, "problem": "failed", "status": "failed",
                    "error": { "name": "ActionError", "message": "GitHub is down" },
                    "calls": { "github.list_issues": 1 },
                    "failures": ["the run failed with ActionError, expected completed"],
                    "transcript": "#0 run_started\n#1 call github.list_issues → error"
                }
            ])),
            &limits,
        );
        assert_eq!(
            text,
            [
                "4 of 5 tests did not pass.",
                "PASS \"closes the stale issues once approved\"",
                &format!("FAIL \"recorded {RUN_ID}\" (failed): it ran, and expectations do not hold:"),
                "  - the output's closed is 2, expected 3",
                "  - github.delete_repo has no recorded response",
                "  The run completed.",
                "  Output: {\"closed\":2}",
                "  Calls: github.close_issue ×2, github.list_issues ×1",
                "  Transcript:",
                "#0 run_started\n#1 call github.list_issues",
                "FAIL \"posts to slack\" (stale): it names actions, answers or args the action set no longer has; it was not run.",
                "  - slack.post_message is not one of the actions",
                "FAIL \"refuses a bad repo\" (incompatible): the workflow's input or signal schemas refuse its input or its signals; it was not run.",
                "  - the input does not match the input schema of triage: repo must be a string",
                "FAIL \"fails when GitHub is down\" (failed): it ran, and expectations do not hold:",
                "  - the run failed with ActionError, expected completed",
                "  The run failed.",
                "  ActionError: GitHub is down",
                "  Calls: github.list_issues ×1",
                "  Transcript:",
                "#0 run_started\n#1 call github.list_issues → error",
            ]
            .join("\n")
        );
        let passed = results(json!([
            { "name": "a", "passed": true, "status": "completed", "calls": {}, "failures": [], "transcript": "#0" },
            { "name": "b", "passed": true, "status": "waiting", "calls": {}, "failures": [], "transcript": "#0" }
        ]));
        assert_eq!(
            describe_test_results(&passed, &limits),
            "All 2 tests passed.\nPASS \"a\"\nPASS \"b\""
        );
        assert_eq!(describe_test_results(&[], &limits), "No test ran.");
    }

    #[test]
    fn namespaces_show_their_display_name_beside_their_name() {
        let list: rewind_api::namespaces::NamespaceList = serde_json::from_value(json!({
            "month": "2026-11",
            "total": 2,
            "namespaces": [
                {
                    "name": "user_2fQ81", "displayName": "Acme Corp",
                    "limits": {}, "exceptions": {},
                    "usage": { "cpu": "1s", "activeRuns": 0, "runsStarted": 1, "workflows": 1 },
                    "createdAt": "2026-11-01T00:00:00Z", "lastActiveAt": "2026-11-18T10:00:00Z"
                },
                {
                    "name": "user_9xZ", "limits": {}, "exceptions": {},
                    "usage": { "cpu": "0s", "activeRuns": 0, "runsStarted": 0, "workflows": 0 },
                    "createdAt": "2026-11-01T00:00:00Z", "lastActiveAt": "2026-11-01T00:00:00Z"
                }
            ]
        }))
        .unwrap();
        let text = describe_namespaces(&list, Some("acme"));
        assert!(
            text.contains("\n- user_2fQ81 (\"Acme Corp\"): 1 run this month"),
            "{text}"
        );
        assert!(text.contains("\n- user_9xZ: 0 runs this month"), "{text}");
        assert!(
            text.ends_with("Give one's name, not its display name in parentheses, as `namespace` to check_workflow, test_workflow and run_workflow."),
            "{text}"
        );
    }

    #[test]
    fn a_refused_version_or_retry_says_what_the_model_may_do() {
        let limits = Limits::default();
        let tests = results(json!([
            {
                "name": "closes nothing without approval", "passed": false, "problem": "failed",
                "status": "completed", "output": { "closed": 1 }, "calls": { "github.close_issue": 1 },
                "failures": ["github.close_issue was called 1 time, expected 0"],
                "transcript": "#0 run_started\n#1 signal approval\n#2 call github.close_issue"
            },
            { "name": "closes once approved", "passed": true, "status": "completed", "calls": {}, "failures": [], "transcript": "#0" }
        ]));
        let failing = [
            "1 of 2 tests did not pass.",
            "FAIL \"closes nothing without approval\" (failed): it ran, and expectations do not hold:",
            "  - github.close_issue was called 1 time, expected 0",
            "  The run completed.",
            "  Output: {\"closed\":1}",
            "  Calls: github.close_issue ×1",
            "  Transcript:",
            "#0 run_started\n#1 signal approval\n#2 call github.close_issue",
            "PASS \"closes once approved\"",
        ]
        .join("\n");
        assert_eq!(
            describe_gate_refused(
                "1 test of 2 did not pass: no version was made",
                &tests,
                "triage",
                true,
                3,
                &limits
            ),
            [
                "No version was made: 1 test of 2 did not pass: no version was made",
                &failing,
                "Every test must pass: there is no way around it. If the code is wrong, fix it, and check it with test_workflow({\"code\":\"…\",\"workflow\":\"triage\"}) before you call update_workflow again. If a test pins a behavior you change on purpose, retire it in `retire` (the behavior is deprecated) or replace it in `replace` (its state or its expected result is out of date), with a reason.",
            ]
            .join("\n")
        );
        assert_eq!(
            describe_gate_refused("1 test of 2 did not pass", &tests, "triage", false, 3, &limits),
            [
                "The suite was not changed: 1 test of 2 did not pass",
                &failing,
                "A test joins the suite only once it passes against the latest version, v3. If the test is wrong, fix it. If the workflow must change to pass it, give its new `code` with the test: the version is made with it once the whole suite passes.",
            ]
            .join("\n")
        );

        let tests = results(json!([
            { "name": "closes once approved", "passed": true, "status": "completed", "calls": {}, "failures": [], "transcript": "#0 ok" },
            {
                "name": "closes nothing without approval", "passed": false, "problem": "failed",
                "status": "completed", "output": { "closed": 1 },
                "calls": { "github.close_issue": 1, "github.list_issues": 1 },
                "failures": ["github.close_issue was called 1 time, expected 0"],
                "transcript": "#0 run_started\n#1 call github.close_issue"
            }
        ]));
        let run_workflow: RunWorkflow = serde_json::from_value(
            json!({ "id": ID, "name": "triage", "version": 3, "digest": VERSION }),
        )
        .unwrap();
        let workflow = run_workflow_reference(&run_workflow);
        assert_eq!(workflow, "triage@v3");
        let refused = |updates| {
            describe_retry_refused(
                RUN_ID,
                "1 test of the suite of triage did not pass against the corrected code",
                &tests,
                Some(&workflow),
                updates,
                &limits,
            )
        };
        assert_eq!(
            refused(true),
            [
                &format!("Run {RUN_ID} was not retried: 1 test of the suite of triage did not pass against the corrected code"),
                "1 of 2 tests did not pass.",
                "PASS \"closes once approved\"",
                "FAIL \"closes nothing without approval\" (failed): it ran, and expectations do not hold:",
                "  - github.close_issue was called 1 time, expected 0",
                "  The run completed.",
                "  Output: {\"closed\":1}",
                "  Calls: github.close_issue ×1, github.list_issues ×1",
                "  Transcript:",
                "#0 run_started\n#1 call github.close_issue",
                "A fix must keep every behavior the suite of triage@v3 pins: there is no way around it. Fix the code, and check it with test_workflow({\"code\":\"…\",\"workflow\":\"triage@v3\"}) before you try again. If a behavior must change on purpose, make the workflow's next version with update_workflow, retiring or replacing its tests with a reason, and run it.",
            ]
            .join("\n")
        );
        assert!(
            refused(false).ends_with("which these tools do not make: tell the person."),
            "{}",
            refused(false)
        );
    }

    #[test]
    fn an_update_says_what_changed_and_how_to_run_it() {
        let limits = Limits::default();
        let patch = PatchWorkflow {
            name: Some(Some("triage".into())),
            description: Some(None),
        };
        let v4 = version(json!({ "version": 4, "digest": "ab".repeat(32), "tests": [
            { "name": "closes once approved", "digest": "11".repeat(32) }
        ] }));
        let made = workflow(json!({}), vec![v4, version(json!({}))]);
        assert_eq!(
            describe_update(&made, &patch, Updated::Version { made: true }, &limits),
            [
                &format!("Updated the workflow triage (id {ID}): named it triage and removed its description."),
                &format!("Made the version v4 (digest abababababab) of the workflow triage (id {ID}): its suite, 1 test, passed against it."),
                &format!("Code starts it from \"rewind:workflows\" once an action set pins it: {}", made.version.declarations.as_ref().unwrap().prompt),
                "Run it with run_workflow({\"workflow\":\"triage\"}).",
            ]
            .join("\n")
        );
        // A workflow without a name shows as its comment's first line, and runs by its id.
        let unnamed = workflow(
            json!({ "name": null, "description": null }),
            vec![version(json!({
                "declarations": null,
                "comment": { "title": "Triage stale issues" }
            }))],
        );
        assert_eq!(
            describe_update(
                &unnamed,
                &PatchWorkflow::default(),
                Updated::Version { made: false },
                &limits
            ),
            [
                format!("The workflow {ID} (no name; it shows as \"Triage stale issues\") already has this content as its version v3: no version was made."),
                format!("Run it with run_workflow({{\"workflow\":\"{ID}\"}})."),
            ]
            .join("\n")
        );
        let tested = workflow(
            json!({ "suite": [
                { "name": "a", "digest": "11".repeat(32), "addedAt": "2026-11-12T10:00:00Z" },
                { "name": "b", "digest": "22".repeat(32), "addedAt": "2026-11-12T10:00:00Z", "retiredAt": "2026-11-13T10:00:00Z", "reason": "gone" }
            ] }),
            vec![version(json!({ "declarations": null }))],
        );
        assert!(
            describe_update(&tested, &PatchWorkflow::default(), Updated::Tests, &limits)
                .starts_with(&format!("Changed the suite of the workflow triage (id {ID}): it holds 1 test, each passing against its version v3, which stays its latest.")),
        );
        assert_eq!(
            describe_update(
                &tested,
                &PatchWorkflow {
                    name: None,
                    description: Some(Some("d".into()))
                },
                Updated::Patched,
                &limits
            ),
            [
                &format!("Updated the workflow triage (id {ID}): gave it a new description."),
                "Its version stays v3.",
                "Run it with run_workflow({\"workflow\":\"triage\"}).",
            ]
            .join("\n")
        );
    }

    #[test]
    fn a_workflow_reads_with_its_suite_the_tests_that_left_it_and_its_versions() {
        let parent_version = "e4".repeat(32);
        let first_version = "a1".repeat(32);
        let inline_digest = format!("4814d92093ac{}", "0".repeat(52));
        let recorded_digest = format!("ca978112ca1b{}", "0".repeat(52));
        let foreign = "b2".repeat(32);
        let old = "3e".repeat(32);
        let retired = "2e".repeat(32);
        let latest = version(json!({
            "base": parent_version,
            "tests": [
                { "name": "closes nothing without approval", "digest": inline_digest },
                { "name": format!("recorded {RUN_ID}"), "digest": recorded_digest },
                { "name": "recorded 0193a5c4-0000-7000-8000-000000000000", "digest": foreign }
            ],
            "replaced": [{
                "name": "closes nothing without approval", "old": old, "new": inline_digest,
                "reason": "the message names the repository"
            }],
            "retired": [{
                "name": "asks before closing", "digest": retired,
                "reason": "under 5 stale issues, it closes without asking"
            }]
        }));
        let parent = version(json!({
            "version": 2, "digest": parent_version, "createdAt": "2026-11-11T10:00:00Z",
            "base": first_version,
            "tests": [{ "name": "asks before closing", "digest": retired }]
        }));
        let first = version(json!({
            "version": 1, "digest": first_version, "createdAt": "2026-11-10T10:00:00Z"
        }));
        let suite = json!([
            { "name": "asks before closing", "digest": retired, "addedAt": "2026-11-11T10:00:00Z",
              "retiredAt": "2026-11-12T10:00:00Z", "reason": "under 5 stale issues, it closes without asking" },
            { "name": "closes nothing without approval", "digest": old, "addedAt": "2026-11-11T10:00:00Z",
              "retiredAt": "2026-11-12T10:00:00Z", "reason": "the message names the repository" },
            { "name": "closes nothing without approval", "digest": inline_digest, "addedAt": "2026-11-12T10:00:00Z" },
            { "name": format!("recorded {RUN_ID}"), "digest": recorded_digest, "addedAt": "2026-11-12T10:00:00Z" },
            { "name": "recorded 0193a5c4-0000-7000-8000-000000000000", "digest": foreign, "addedAt": "2026-11-12T10:00:00Z" }
        ]);
        let triage = workflow(
            json!({ "suite": suite, "source": { "workflow": "wf_0rg4n1z4t10n", "version": 7 } }),
            vec![latest, parent, first],
        );
        let inline = json!({
            "name": "closes nothing without approval",
            "input": { "repo": "acme/api" },
            "world": { "actions": { "github.list_issues": [{ "returns": [{ "number": 4 }] }] } },
            "signals": [{ "name": "approval", "payload": { "close": false } }],
            "expect": { "output": { "closed": 0 }, "calls": [{ "action": "github.close_issue", "times": 0 }] }
        });
        let recorded = json!({
            "name": format!("recorded {RUN_ID}"),
            "input": { "repo": "acme/api" },
            "seed": "07".repeat(32),
            "world": { "cassette": {
                "v": 1,
                "matching": "by_args",
                "calls": [
                    { "action": "github.list_issues", "args": { "repo": "acme/api" }, "outcome": { "result": [] } },
                    { "action": "github.close_issue", "args": { "number": 4 }, "outcome": { "result": {} } },
                    { "action": "github.close_issue", "args": { "number": 5 }, "outcome": { "result": {} } }
                ],
                "signals": [{ "name": "approval", "payload": { "close": true }, "elapsed_ms": 3_600_000 }]
            } },
            "expect": { "status": "completed", "output": { "closed": 2 } },
            "from": { "run": RUN_ID, "version": parent_version, "namespace": "user_2fQ81" }
        });
        let details = WorkflowDetails {
            workflow: triage.clone(),
            tests: vec![
                ReadTest {
                    name: "closes nothing without approval".into(),
                    digest: inline_digest.clone(),
                    test: Some(serde_json::from_value(inline).unwrap()),
                    unreadable: None,
                },
                ReadTest {
                    name: format!("recorded {RUN_ID}"),
                    digest: recorded_digest.clone(),
                    test: Some(serde_json::from_value(recorded).unwrap()),
                    unreadable: None,
                },
                ReadTest {
                    name: "recorded 0193a5c4-0000-7000-8000-000000000000".into(),
                    digest: foreign.clone(),
                    test: None,
                    unreadable: Some(
                        "reading this test needs runs:read in the namespace it was recorded from"
                            .into(),
                    ),
                },
            ],
            updates: true,
        };
        let shown = &triage.version;
        assert_eq!(
            describe_workflow_details(&details, &Limits::default()),
            [
                format!("The workflow triage (id {ID}), of the namespace user_2fQ81."),
                "It is a fork of the organization's workflow wf_0rg4n1z4t10n, whose version v7 it last took.".to_owned(),
                "Description: Triage the stale issues of a repository, after approval.".to_owned(),
                "Its version v3 of 3 versions (digest 4be1c07a9d3e), made 2026-11-12T10:00:00Z, from v2.".to_owned(),
                "Action set: 9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4".to_owned(),
                format!("Input schema: {}", json(shown.input.as_ref().unwrap())),
                format!("Output schema: {}", json(shown.output.as_ref().unwrap())),
                "Signals:".to_owned(),
                format!("- \"approval\": {}", json(&shown.signals.as_ref().unwrap()["approval"])),
                "Secrets its runs use: {\"github\":\"ns/github\"}".to_owned(),
                format!("Code starts it from \"rewind:workflows\" once an action set pins it: {}", shown.declarations.as_ref().unwrap().prompt),
                "Code:".to_owned(),
                "export default async () => ({ closed: 0 });".to_owned(),
                "Its suite, 3 tests, which its next version must pass (give a digest in update_workflow's `tests` or `replace` to reuse a test):".to_owned(),
                format!("Test \"closes nothing without approval\" ({inline_digest}):"),
                // The order the server writes a test's fields in.
                "{\"name\":\"closes nothing without approval\",\"input\":{\"repo\":\"acme/api\"},\"world\":{\"actions\":{\"github.list_issues\":[{\"returns\":[{\"number\":4}]}]}},\"signals\":[{\"name\":\"approval\",\"payload\":{\"close\":false}}],\"expect\":{\"output\":{\"closed\":0},\"calls\":[{\"action\":\"github.close_issue\",\"times\":0}]}}".to_owned(),
                format!("Test \"recorded {RUN_ID}\" ({recorded_digest}), recorded from run {RUN_ID} of v2:"),
                format!(
                    "{{\"name\":\"recorded {RUN_ID}\",\"input\":{{\"repo\":\"acme/api\"}},\"seed\":\"{}\",\"world\":{{\"cassette\":{{\"summary\":\"3 recorded calls matched by_args (github.list_issues ×1, github.close_issue ×2), 1 signal (approval)\"}}}},\"expect\":{{\"status\":\"completed\",\"output\":{{\"closed\":2}}}},\"from\":{{\"run\":\"{RUN_ID}\",\"version\":\"{parent_version}\",\"namespace\":\"user_2fQ81\"}}}}",
                    "07".repeat(32)
                ),
                format!("Test \"recorded 0193a5c4-0000-7000-8000-000000000000\" ({foreign}): it cannot be read: reading this test needs runs:read in the namespace it was recorded from"),
                "Tests that left its suite, and why:".to_owned(),
                "- \"asks before closing\" (2e2e2e2e2e2e), left 2026-11-12T10:00:00Z: under 5 stale issues, it closes without asking".to_owned(),
                "- \"closes nothing without approval\" (3e3e3e3e3e3e), left 2026-11-12T10:00:00Z: the message names the repository".to_owned(),
                "Its versions, newest first:".to_owned(),
                "- v3, made 2026-11-12T10:00:00Z, from v2: 3 tests; replaced \"closes nothing without approval\", retired \"asks before closing\".".to_owned(),
                "- v2, made 2026-11-11T10:00:00Z, from v1: 1 test.".to_owned(),
                "- v1, made 2026-11-10T10:00:00Z, the first version: 0 tests.".to_owned(),
            ]
            .join("\n")
        );

        // A workflow of the organization without a name, a comment or an interface.
        let bare = workflow(
            json!({ "name": null, "description": null, "namespace": null }),
            vec![version(json!({
                "version": 1, "input": null, "output": null, "signals": null, "secrets": {},
                "declarations": null
            }))],
        );
        let text = describe_workflow_details(
            &WorkflowDetails {
                workflow: bare,
                tests: Vec::new(),
                updates: false,
            },
            &Limits::default(),
        );
        assert!(
            text.starts_with(&format!(
                "The workflow {ID} (no name; it shows as wf-7k2m9q), of the organization.\nIts version v1 of 1 version (digest 4be1c07a9d3e), made 2026-11-12T10:00:00Z.\nAction set: 9f2c"
            )),
            "{text}"
        );
        assert!(
            text.contains("\nInterface: none. It takes any input, returns any output and accepts any signal name.\nCode:\n"),
            "{text}"
        );
        assert!(
            text.contains("\nIts suite is empty: it has no test.\n"),
            "{text}"
        );
    }

    #[test]
    fn a_recording_lists_its_tests_and_the_runs_it_skipped() {
        let limits = Limits::default();
        let recorded: RecordedTests = serde_json::from_value(json!({
            "tests": [
                {
                    "digest": "9f".repeat(32), "name": format!("recorded {RUN_ID}"),
                    "path": "github.list_issues · signal approval · github.close_issue×n → completed",
                    "run": RUN_ID, "status": "completed"
                },
                {
                    "digest": "8e".repeat(32), "name": "recorded r-2",
                    "path": "github.list_issues → failed", "run": "r-2", "status": "failed"
                }
            ],
            "skipped": [{ "run": "r-big", "reason": "its test would be larger than 4 MiB" }]
        }))
        .unwrap();
        assert_eq!(
            describe_recorded("triage", &recorded, true, &limits),
            [
                "Recorded 2 tests from the runs of triage, one per path:".to_owned(),
                format!("- {} \"recorded {RUN_ID}\", from run {RUN_ID} (completed): github.list_issues · signal approval · github.close_issue×n → completed", "9f".repeat(32)),
                format!("- {} \"recorded r-2\", from run r-2 (failed): github.list_issues → failed", "8e".repeat(32)),
                "Skipped 1 run:".to_owned(),
                "- r-big: its test would be larger than 4 MiB".to_owned(),
                "They join no suite yet: pass their digests in update_workflow's `tests` to add them to the suite of triage, or { \"test\": \"<digest>\", \"expect\": {…} } to give a failed run the expectation it should have met.".to_owned(),
            ]
            .join("\n")
        );
        assert!(
            describe_recorded("triage", &recorded, false, &limits).ends_with(
                "They join no suite: pass their digests in test_workflow's `tests` to run them against a code, or { \"test\": \"<digest>\", \"expect\": {…} } to give a failed run the expectation it should have met."
            )
        );
        let none = RecordedTests {
            tests: vec![],
            skipped: vec![],
        };
        assert_eq!(
            describe_recorded("triage", &none, true, &limits),
            "No test was recorded: no completed or failed run of triage was selected."
        );
    }

    #[test]
    fn runs_say_which_workflow_they_execute_and_which_run_started_them() {
        let run: Run = serde_json::from_value(json!({
            "id": RUN_ID, "handle": "user_2fQ81/triage/06bqx3k9f2mt", "namespace": "user_2fQ81",
            "status": "completed", "codeDigest": "ab".repeat(32),
            "actionSet": "t", "epoch": 1, "mode": "live", "labels": {}, "traceId": "0", "historyLen": 4,
            "createdAt": "2026-11-12T10:00:00Z", "updatedAt": "2026-11-12T10:00:01Z",
            "expiresAt": "2026-12-12T10:00:00Z",
            "workflow": { "id": ID, "name": "triage", "digest": VERSION, "version": 3 },
            "startedBy": {
                "run": "source-1", "handle": "user_2fQ81/wf-0a1b2c/06bqx3k9f2ms", "seq": 2,
                "detached": true
            }
        }))
        .unwrap();
        // A run is named by its handle, and so are the runs it names; its workflow by its name,
        // its id and its version.
        assert_eq!(
            describe_run(&run, &RunExtras::default(), &Limits::default())
                .lines()
                .next()
                .unwrap(),
            format!(
                "Run user_2fQ81/triage/06bqx3k9f2mt (a run of the workflow triage@v3, id {ID}; started by run user_2fQ81/wf-0a1b2c/06bqx3k9f2ms, detached) completed."
            )
        );
        let summary: RunSummary = serde_json::from_value(json!({
            "id": "r2", "handle": "user_2fQ81/wf-7k2m9q/06bqx3k9f2mv", "namespace": "user_2fQ81",
            "status": "waiting", "quarantined": false, "codeDigest": "s",
            "actionSet": "t", "labels": {}, "startedAt": "2026-11-12T10:00:00Z",
            "lastActivityAt": "2026-11-12T10:00:00Z", "calls": 1, "errors": 0, "historyLen": 3,
            "waitingFor": [], "workflow": { "id": ID, "digest": VERSION, "version": 1 },
            "startedBy": "r1"
        }))
        .unwrap();
        assert_eq!(
            describe_runs(&[summary], false, None),
            [
                "1 run, most recent first:",
                &format!("- user_2fQ81/wf-7k2m9q/06bqx3k9f2mv: waiting, started 2026-11-12T10:00:00Z, 1 call, a run of the workflow {ID}@v1, started by r1"),
            ]
            .join("\n")
        );
    }

    #[test]
    fn a_check_says_which_workflow_already_has_the_code() {
        let checked: CheckResult = serde_json::from_value(json!({
            "ok": true, "diagnostics": [],
            "workflow": { "id": ID, "name": "triage", "version": 3, "digest": VERSION }
        }))
        .unwrap();
        assert_eq!(
            describe_check(&checked, &Limits::default()),
            format!(
                "The code type-checks.\nThis code is already the version v3 of the workflow triage (id {ID}): running it is a run of that workflow."
            )
        );
    }
}
