//! What the agent tools say to the model: a port of the SDK's
//! `sdk/typescript/src/agent-tools/render.ts` and `bound.ts`, on the same DTOs, so that a model
//! reads in the evals what it reads through the SDK. Plain text, short, bounded, pointing at
//! the next useful call.

use std::collections::BTreeSet;

use rewind_api::common::{Callsite, Payload, RunStatus};
use rewind_api::runs::{FailedCall, Run, RunError, RunSummary, Waiting};
use rewind_api::scripts::{CheckResult, Diagnostic, Severity};
use rewind_api::tasks::{ParkedReason, TaskState};
use rewind_api::testing::{Test, TestProblem, TestResult};
use rewind_api::toolsets::ToolMatch;
use rewind_api::workflows::{RecordedTests, Workflow};
use serde_json::{Value, json};

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
    /// A whole result: room for the parts above, and a last bound on any text left unbounded.
    pub total: usize,
}

impl Limits {
    pub fn for_output(output: usize) -> Self {
        let trace = (output / 2).max(4096);
        Self {
            output,
            trace,
            value: (output / 64).max(1024),
            line: 2048,
            total: output + trace + 64 * 1024,
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
    let mut lines = vec![headline(run, limits)];
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
        run.parent
            .as_ref()
            .map(|parent| format!("a fork of run {}", parent.run)),
        run.workflow.as_ref().map(|workflow| {
            format!(
                "a run of the workflow {}",
                workflow_ref(&workflow.name, &workflow.version)
            )
        }),
        run.started_by.as_ref().map(|by| {
            format!(
                "started by run {}{}",
                by.run,
                if by.detached { ", detached" } else { "" }
            )
        }),
    ]
    .into_iter()
    .flatten()
    .collect();
    let name = if about.is_empty() {
        format!("Run {}", run.id)
    } else {
        format!("Run {} ({})", run.id, about.join(", "))
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
        RunStatus::Pending => format!("{name} is pending: it has not started executing yet."),
        RunStatus::Active => {
            if let Some(quarantine) = &run.quarantine {
                let cause = match quarantine.cause {
                    rewind_api::runs::QuarantineCause::NonDeterminism => "non_determinism",
                    rewind_api::runs::QuarantineCause::RepeatedFailures => "repeated_failures",
                };
                return format!(
                    "{name} is quarantined ({cause}): {}. It waits for the platform to be fixed; it is not a fault of the script.",
                    truncate_end(&quarantine.reason, limits.value)
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

fn abort_kind(kind: rewind_api::common::AbortKind) -> &'static str {
    match kind {
        rewind_api::common::AbortKind::LimitExceeded => "limit_exceeded",
        rewind_api::common::AbortKind::NonDeterminism => "non_determinism",
        rewind_api::common::AbortKind::Cancelled => "cancelled",
        rewind_api::common::AbortKind::InvalidScript => "invalid_script",
        rewind_api::common::AbortKind::Internal => "internal",
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
            call.tool,
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

fn failure_kind(kind: rewind_api::common::ToolFailureKind) -> &'static str {
    match kind {
        rewind_api::common::ToolFailureKind::Error => "error",
        rewind_api::common::ToolFailureKind::Unavailable => "unavailable",
        rewind_api::common::ToolFailureKind::Timeout => "timeout",
        rewind_api::common::ToolFailureKind::OutcomeUnknown => "outcome_unknown",
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
pub fn entry_text(entry: &rewind_api::events::Entry) -> String {
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
    if result.ok {
        return if warnings == 0 {
            "The script type-checks.".to_owned()
        } else {
            format!(
                "The script type-checks, with {}:\n{}",
                counts.join(" and "),
                diagnostics_text(&result.diagnostics, limits)
            )
        };
    }
    format!(
        "The script does not type-check: {}.\n{}",
        counts.join(" and "),
        diagnostics_text(&result.diagnostics, limits)
    )
}

/// A script refused before any run exists.
pub fn describe_refusal(detail: &str, diagnostics: &[Diagnostic], limits: &Limits) -> String {
    let text = format!(
        "The script was refused before anything ran: {}",
        truncate_end(detail, limits.value)
    );
    if diagnostics.is_empty() {
        text
    } else {
        format!("{text}\n{}", diagnostics_text(diagnostics, limits))
    }
}

/// The diagnostics, in order, as many as the room of a trace holds.
pub fn diagnostics_text(diagnostics: &[Diagnostic], limits: &Limits) -> String {
    let mut shown: Vec<String> = Vec::new();
    let mut bytes = 0;
    for d in diagnostics {
        let location = d
            .location
            .map(|l| format!("script.ts:{}:{} ", l.line, l.column))
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
        if bytes > limits.trace && !shown.is_empty() {
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
/// its failures, how its run ended, its calls and its trace. The traces share the room of one
/// trace.
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
    let traced = failing
        .iter()
        .filter(|result| !result.trace.trim().is_empty())
        .count();
    let trace_room = (limits.trace / traced.max(1)).max(limits.line);
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
            lines.push("  No tool was called.".to_owned());
        } else {
            let calls: Vec<String> = result
                .calls
                .iter()
                .map(|(tool, count)| format!("{tool} ×{count}"))
                .collect();
            lines.push(format!("  Calls: {}", calls.join(", ")));
        }
        let trace = result.trace.trim_end();
        if !trace.trim().is_empty() {
            lines.push("  Trace:".to_owned());
            lines.push(truncate_middle(trace, trace_room));
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
            "it names tools, answers or args the toolset no longer has; it was not run."
        }
        TestProblem::Incompatible => {
            "the workflow's input or signal schemas refuse its input or its signals; it was not run."
        }
        TestProblem::Failed => "it ran, and expectations do not hold:",
    }
}

/// Tests that refused a save of a workflow: the outcome of each, and what the model may do.
pub fn describe_save_refused(
    detail: &str,
    results: &[TestResult],
    workflow: &str,
    limits: &Limits,
) -> String {
    [
        format!(
            "The workflow was not saved: {}",
            truncate_end(detail, limits.value)
        ),
        describe_test_results(results, limits),
        format!(
            "Every test must pass: there is no way around it. If the script is wrong, fix it, and check it with test_script({}) before you save it again. If a test pins a behavior you change on purpose, retire it in `retire` (the behavior is deprecated) or update it in `update` (its state or its expected result is out of date), with a reason.",
            args(&[("code", json!("…")), ("workflow", json!(workflow))])
        ),
    ]
    .join("\n")
}

/// Tests that refused the fork of a run of a workflow: the outcome of each, and what the model
/// may do.
pub fn describe_retry_refused(
    run_id: &str,
    detail: &str,
    results: &[TestResult],
    workflow: Option<&str>,
    limits: &Limits,
) -> String {
    let suite = workflow.map_or_else(
        || "the suite of its workflow".to_owned(),
        |workflow| format!("the suite of {workflow}"),
    );
    let check = workflow.map_or_else(
        || "test_script".to_owned(),
        |workflow| {
            format!(
                "test_script({})",
                args(&[("code", json!("…")), ("workflow", json!(workflow))])
            )
        },
    );
    [
        format!(
            "Run {run_id} was not retried: {}",
            truncate_end(detail, limits.value)
        ),
        describe_test_results(results, limits),
        format!(
            "A fix must keep every behavior {suite} pins: there is no way around it. Fix the script, and check it with {check} before you try again. If a behavior must change on purpose, save a new version of the workflow with save_workflow, retiring or updating its tests with a reason, and start a new run of it."
        ),
    ]
    .join("\n")
}

/// A workflow saved: how the model's scripts use it, and its declaration.
pub fn describe_workflow(workflow: &Workflow, limits: &Limits) -> String {
    let mut lines = vec![
        format!(
            "Saved the workflow {}. Your next scripts can import it:",
            workflow_ref(&workflow.name, &workflow.version)
        ),
        format!("import {{ {} }} from \"rewind:workflows\";", workflow.name),
    ];
    let prompt = workflow.declarations.prompt.trim();
    if !prompt.is_empty() {
        lines.push(truncate_end(prompt, limits.value));
    }
    let dts = workflow.declarations.dts.trim();
    if !dts.is_empty() {
        lines.push("Its declaration:".to_owned());
        lines.push(truncate_end(dts, limits.trace));
    }
    lines.join("\n")
}

/// A workflow refused: why, and the diagnostics of its script.
pub fn describe_workflow_refusal(
    detail: &str,
    diagnostics: &[Diagnostic],
    limits: &Limits,
) -> String {
    let text = format!(
        "The workflow was not saved: {}",
        truncate_end(detail, limits.value)
    );
    if diagnostics.is_empty() {
        text
    } else {
        format!("{text}\n{}", diagnostics_text(diagnostics, limits))
    }
}

/// `triage@9f2c4e1a07b3`: a workflow's name and the first 12 digits of its version.
pub fn workflow_ref(name: &str, version: &str) -> String {
    format!("{name}@{}", version.get(..12).unwrap_or(version))
}

/// What `get_workflow` reads: the latest version, its suite with each test, and the lineage.
#[derive(Debug, Clone)]
pub struct WorkflowDetails {
    /// Every version, newest first.
    pub versions: Vec<Workflow>,
    /// The source of the latest version.
    pub source: String,
    /// The tests of the latest version's suite, in its order.
    pub tests: Vec<SuiteEntry>,
}

/// A test of a suite as `get_workflow` reads it: the test, or why it cannot be read.
#[derive(Debug, Clone)]
pub struct SuiteEntry {
    pub name: String,
    pub digest: String,
    pub test: Option<Test>,
    pub unreadable: Option<String>,
}

/// A workflow, for the model to repair or extend it: its source, schemas, suite and lineage.
pub fn describe_workflow_details(details: &WorkflowDetails, limits: &Limits) -> String {
    let Some(latest) = details.versions.first() else {
        return "The workflow has no version.".to_owned();
    };
    let mut lines = vec![format!(
        "The workflow {}, saved {}: its latest version, of {}.",
        workflow_ref(&latest.name, &latest.version),
        latest.created_at,
        plural(details.versions.len(), "version")
    )];
    if let Some(parent) = &latest.parent {
        lines.push(format!(
            "It derives from the version {}.",
            version_ref(parent, &details.versions)
        ));
    }
    if let Some(description) = &latest.description {
        lines.push(format!(
            "Description: {}",
            truncate_end(description, limits.value)
        ));
    }
    lines.push(format!(
        "Input schema: {}",
        truncate_end(&json(&latest.input), limits.value)
    ));
    lines.push(format!(
        "Output schema: {}",
        truncate_end(&json(&latest.output), limits.value)
    ));
    if latest.signals.is_empty() {
        lines.push("Signals: none.".to_owned());
    } else {
        lines.push("Signals:".to_owned());
        for (name, schema) in &latest.signals {
            lines.push(format!(
                "- {}: {}",
                quoted(name),
                truncate_end(&json(schema), limits.value)
            ));
        }
    }
    lines.push("Source:".to_owned());
    lines.push(truncate_middle(details.source.trim_end(), limits.trace));

    if details.tests.is_empty() {
        lines.push("Its suite is empty: it has no test.".to_owned());
    } else {
        lines.push(format!(
            "Its suite, {}, which every version derived from it must pass (give a digest in save_workflow's `tests` or `update` to reuse a test):",
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
                .origin
                .as_ref()
                .map(|origin| {
                    format!(
                        ", recorded from run {} of {}",
                        origin.run,
                        version_ref(&origin.version, &details.versions)
                    )
                })
                .unwrap_or_default();
            lines.push(format!("{head}{origin}:"));
            lines.push(truncate_end(&readable_test(test), room));
        }
    }

    lines.push("Lineage, newest first:".to_owned());
    for version in &details.versions {
        let from = version.parent.as_ref().map_or_else(
            || ", the first version".to_owned(),
            |parent| format!(", from {}", version_ref(parent, &details.versions)),
        );
        lines.push(format!(
            "- {}, saved {}{from}: {}.",
            workflow_ref(&version.name, &version.version),
            version.created_at,
            plural(version.tests.len(), "test")
        ));
        for updated in &version.updated {
            lines.push(format!(
                "  updated {}: {}",
                quoted(&truncate_end(&updated.name, limits.line)),
                truncate_end(&updated.reason, limits.value)
            ));
        }
        for retired in &version.retired {
            lines.push(format!(
                "  retired {}: {}",
                quoted(&truncate_end(&retired.name, limits.line)),
                truncate_end(&retired.reason, limits.value)
            ));
        }
    }
    lines.join("\n")
}

/// `triage@4be1c07a9d3e` when the version is one of `versions`, otherwise its first 12 digits.
fn version_ref(version: &str, versions: &[Workflow]) -> String {
    versions
        .iter()
        .find(|candidate| candidate.version == version)
        .map_or_else(
            || version.get(..12).unwrap_or(version).to_owned(),
            |known| workflow_ref(&known.name, &known.version),
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
    // In the order each tool is first called, as a JavaScript `Map` keeps them.
    let mut by_tool: Vec<(String, usize)> = Vec::new();
    for call in &calls {
        let tool = call["tool"].as_str().unwrap_or("?");
        match by_tool.iter_mut().find(|(known, _)| known == tool) {
            Some((_, count)) => *count += 1,
            None => by_tool.push((tool.to_owned(), 1)),
        }
    }
    let tools = by_tool
        .iter()
        .map(|(tool, count)| format!("{tool} ×{count}"))
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
        if tools.is_empty() {
            String::new()
        } else {
            format!(" ({tools})")
        },
        plural(signals.len(), "signal"),
        if names.is_empty() {
            String::new()
        } else {
            format!(" ({})", names.join(", "))
        }
    )
}

/// The tests a recording stored, and the runs it left out.
pub fn describe_recorded(workflow: &str, recorded: &RecordedTests, limits: &Limits) -> String {
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
        lines.push(format!(
            "They join no suite yet: pass their digests in save_workflow's `tests` to add them to the suite of {workflow}'s next version, or {{ \"from\": \"<digest>\", \"expect\": {{…}} }} to give a failed run the expectation it should have met."
        ));
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
        if let Some(workflow) = &run.workflow {
            parts.push(format!("a run of the workflow {workflow}"));
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
        RunStatus::Active => "was still waiting at the end of the test",
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
    rewind_core::canonical_json(value)
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
    fn every_text_a_script_a_server_or_a_tool_wrote_is_bounded() {
        let limits = Limits::for_output(64 * 1024);
        let long = "m".repeat(3_000_000);
        let run: Run = serde_json::from_value(json!({
            "id": "r", "namespace": "alice", "status": "failed", "script": "s", "toolset": "t", "epoch": 1,
            "mode": "live", "labels": {}, "traceId": "0", "historyLen": 3,
            "createdAt": "2026-11-12T00:00:00Z", "updatedAt": "2026-11-12T00:00:00Z",
            "expiresAt": "2026-11-13T00:00:00Z",
            "error": {
                "name": long, "message": long, "frame": long,
                "failedCall": {
                    "seq": 1, "tool": "crm.find", "args": { "value": {}, "digest": "d", "bytes": 2, "preview": "{}" },
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
            "error": { "name": long, "message": long }, "calls": {}, "trace": ""
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
        // Diagnostics take the room of a trace, then say how many are left out.
        let many: Vec<Diagnostic> = (0..1000)
            .map(|n| diagnostic(&format!("error {n}")))
            .collect();
        let text = diagnostics_text(&many, &limits);
        assert!(text.len() <= limits.trace + limits.value + limits.line + 100);
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

    // The texts of the SDK's tests (`sdk/typescript/test/agent-tools.test.ts`), from the same
    // answers of the server.

    const RUN_ID: &str = "0193a5c4-7d2e-7c1a-9b1e-3f6a2d9c8e10";
    const VERSION: &str = "4be1c07a9d3e0000000000000000000000000000000000000000000000000000";

    fn results(value: Value) -> Vec<TestResult> {
        serde_json::from_value(value).unwrap()
    }

    /// The SDK's `workflow()` fixture.
    fn workflow(overrides: Value) -> Workflow {
        let mut workflow = json!({
            "name": "triage",
            "version": VERSION,
            "description": "Triage the stale issues of a repository, after approval.",
            "toolset": "9f2c4e1a07b3d5f8a1c2e3b4d5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4",
            "script": "cd".repeat(32),
            "input": { "type": "object", "properties": { "repo": { "type": "string" } }, "required": ["repo"] },
            "output": { "type": "object", "properties": { "closed": { "type": "integer" } }, "required": ["closed"] },
            "signals": { "approval": { "type": "object", "properties": { "close": { "type": "boolean" } }, "required": ["close"] } },
            "labels": { "conversation": "c-42" },
            "createdAt": "2026-11-12T10:00:00Z",
            "declarations": {
                "dts": "declare module \"rewind:workflows\" {\n  export const triage: Workflow<{ repo: string }, { closed: number }, { approval: { close: boolean } }>;\n}",
                "prompt": "triage.start({ repo: string }) → run · run.signal(\"approval\", { close: boolean }) · run.result() → { closed: number } · Triage the stale issues of a repository, after approval."
            }
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
                    "trace": "#0 run_started (a passing test's trace is not shown)"
                },
                {
                    "name": format!("recorded {RUN_ID}"), "passed": false, "problem": "failed", "status": "completed",
                    "output": { "closed": 2 }, "calls": { "github.close_issue": 2, "github.list_issues": 1 },
                    "failures": ["the output's closed is 2, expected 3", "github.delete_repo has no recorded response"],
                    "trace": "#0 run_started\n#1 tool github.list_issues"
                },
                {
                    "name": "posts to slack", "passed": false, "problem": "stale", "calls": {},
                    "failures": ["slack.post_message is not one of the tools"], "trace": ""
                },
                {
                    "name": "refuses a bad repo", "passed": false, "problem": "incompatible", "calls": {},
                    "failures": ["the input does not match the input schema of triage: repo must be a string"], "trace": ""
                },
                {
                    "name": "fails when GitHub is down", "passed": false, "problem": "failed", "status": "failed",
                    "error": { "name": "ToolError", "message": "GitHub is down" },
                    "calls": { "github.list_issues": 1 },
                    "failures": ["the run failed with ToolError, expected completed"],
                    "trace": "#0 run_started\n#1 tool github.list_issues → error"
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
                "  Trace:",
                "#0 run_started\n#1 tool github.list_issues",
                "FAIL \"posts to slack\" (stale): it names tools, answers or args the toolset no longer has; it was not run.",
                "  - slack.post_message is not one of the tools",
                "FAIL \"refuses a bad repo\" (incompatible): the workflow's input or signal schemas refuse its input or its signals; it was not run.",
                "  - the input does not match the input schema of triage: repo must be a string",
                "FAIL \"fails when GitHub is down\" (failed): it ran, and expectations do not hold:",
                "  - the run failed with ToolError, expected completed",
                "  The run failed.",
                "  ToolError: GitHub is down",
                "  Calls: github.list_issues ×1",
                "  Trace:",
                "#0 run_started\n#1 tool github.list_issues → error",
            ]
            .join("\n")
        );
        let passed = results(json!([
            { "name": "a", "passed": true, "status": "completed", "calls": {}, "failures": [], "trace": "#0" },
            { "name": "b", "passed": true, "status": "active", "calls": {}, "failures": [], "trace": "#0" }
        ]));
        assert_eq!(
            describe_test_results(&passed, &limits),
            "All 2 tests passed.\nPASS \"a\"\nPASS \"b\""
        );
        assert_eq!(describe_test_results(&[], &limits), "No test ran.");
    }

    #[test]
    fn a_refused_save_or_retry_says_what_the_model_may_do() {
        let limits = Limits::default();
        let tests = results(json!([
            {
                "name": "closes nothing without approval", "passed": false, "problem": "failed",
                "status": "completed", "output": { "closed": 1 }, "calls": { "github.close_issue": 1 },
                "failures": ["github.close_issue was called 1 time, expected 0"],
                "trace": "#0 run_started\n#1 signal approval\n#2 tool github.close_issue"
            },
            { "name": "closes once approved", "passed": true, "status": "completed", "calls": {}, "failures": [], "trace": "#0" }
        ]));
        assert_eq!(
            describe_save_refused(
                "1 test of 2 did not pass: the version was not saved",
                &tests,
                "triage",
                &limits
            ),
            [
                "The workflow was not saved: 1 test of 2 did not pass: the version was not saved",
                "1 of 2 tests did not pass.",
                "FAIL \"closes nothing without approval\" (failed): it ran, and expectations do not hold:",
                "  - github.close_issue was called 1 time, expected 0",
                "  The run completed.",
                "  Output: {\"closed\":1}",
                "  Calls: github.close_issue ×1",
                "  Trace:",
                "#0 run_started\n#1 signal approval\n#2 tool github.close_issue",
                "PASS \"closes once approved\"",
                "Every test must pass: there is no way around it. If the script is wrong, fix it, and check it with test_script({\"code\":\"…\",\"workflow\":\"triage\"}) before you save it again. If a test pins a behavior you change on purpose, retire it in `retire` (the behavior is deprecated) or update it in `update` (its state or its expected result is out of date), with a reason.",
            ]
            .join("\n")
        );

        let tests = results(json!([
            { "name": "closes once approved", "passed": true, "status": "completed", "calls": {}, "failures": [], "trace": "#0 ok" },
            {
                "name": "closes nothing without approval", "passed": false, "problem": "failed",
                "status": "completed", "output": { "closed": 1 },
                "calls": { "github.close_issue": 1, "github.list_issues": 1 },
                "failures": ["github.close_issue was called 1 time, expected 0"],
                "trace": "#0 run_started\n#1 tool github.close_issue"
            }
        ]));
        let workflow = workflow_ref("triage", VERSION);
        assert_eq!(
            describe_retry_refused(
                RUN_ID,
                "1 test of the suite of triage did not pass against the corrected script",
                &tests,
                Some(&workflow),
                &limits
            ),
            [
                &format!("Run {RUN_ID} was not retried: 1 test of the suite of triage did not pass against the corrected script"),
                "1 of 2 tests did not pass.",
                "PASS \"closes once approved\"",
                "FAIL \"closes nothing without approval\" (failed): it ran, and expectations do not hold:",
                "  - github.close_issue was called 1 time, expected 0",
                "  The run completed.",
                "  Output: {\"closed\":1}",
                "  Calls: github.close_issue ×1, github.list_issues ×1",
                "  Trace:",
                "#0 run_started\n#1 tool github.close_issue",
                "A fix must keep every behavior the suite of triage@4be1c07a9d3e pins: there is no way around it. Fix the script, and check it with test_script({\"code\":\"…\",\"workflow\":\"triage@4be1c07a9d3e\"}) before you try again. If a behavior must change on purpose, save a new version of the workflow with save_workflow, retiring or updating its tests with a reason, and start a new run of it.",
            ]
            .join("\n")
        );
    }

    #[test]
    fn a_saved_workflow_says_how_to_import_it() {
        let saved = workflow(json!({}));
        assert_eq!(
            describe_workflow(&saved, &Limits::default()),
            [
                "Saved the workflow triage@4be1c07a9d3e. Your next scripts can import it:",
                "import { triage } from \"rewind:workflows\";",
                &saved.declarations.prompt,
                "Its declaration:",
                &saved.declarations.dts,
            ]
            .join("\n")
        );
        assert_eq!(
            describe_workflow_refusal(
                "the workflow name \"class\" is a reserved word of JavaScript",
                &[],
                &Limits::default()
            ),
            "The workflow was not saved: the workflow name \"class\" is a reserved word of JavaScript"
        );
    }

    #[test]
    fn a_workflow_reads_with_its_suite_and_its_lineage() {
        let parent_version = "e4".repeat(32);
        let first_version = "a1".repeat(32);
        let inline_digest = format!("4814d92093ac{}", "0".repeat(52));
        let recorded_digest = format!("ca978112ca1b{}", "0".repeat(52));
        let foreign = "b2".repeat(32);
        let latest = workflow(json!({
            "parent": parent_version,
            "tests": [
                { "name": "closes nothing without approval", "digest": inline_digest },
                { "name": format!("recorded {RUN_ID}"), "digest": recorded_digest },
                { "name": "recorded 0193a5c4-0000-7000-8000-000000000000", "digest": foreign }
            ],
            "updated": [{
                "name": "closes nothing without approval", "from": "3e".repeat(32), "to": inline_digest,
                "reason": "the message names the repository"
            }],
            "retired": [{
                "name": "asks before closing", "digest": "2e".repeat(32),
                "reason": "under 5 stale issues, it closes without asking"
            }]
        }));
        let parent = workflow(json!({
            "version": parent_version,
            "createdAt": "2026-11-11T10:00:00Z",
            "parent": first_version,
            "tests": [{ "name": "asks before closing", "digest": "2e".repeat(32) }]
        }));
        let first = workflow(json!({
            "version": first_version, "createdAt": "2026-11-10T10:00:00Z", "tests": []
        }));
        let inline = json!({
            "name": "closes nothing without approval",
            "input": { "repo": "acme/api" },
            "world": { "tools": { "github.list_issues": [{ "returns": [{ "number": 4 }] }] } },
            "signals": [{ "name": "approval", "payload": { "close": false } }],
            "expect": { "output": { "closed": 0 }, "calls": [{ "tool": "github.close_issue", "times": 0 }] }
        });
        let recorded = json!({
            "name": format!("recorded {RUN_ID}"),
            "input": { "repo": "acme/api" },
            "seed": "07".repeat(32),
            "world": { "cassette": {
                "v": 1,
                "matching": "by_args",
                "calls": [
                    { "tool": "github.list_issues", "args": { "repo": "acme/api" }, "outcome": { "result": [] } },
                    { "tool": "github.close_issue", "args": { "number": 4 }, "outcome": { "result": {} } },
                    { "tool": "github.close_issue", "args": { "number": 5 }, "outcome": { "result": {} } }
                ],
                "signals": [{ "name": "approval", "payload": { "close": true }, "elapsed_ms": 3_600_000 }]
            } },
            "expect": { "status": "completed", "output": { "closed": 2 } },
            "origin": { "run": RUN_ID, "version": parent_version, "namespace": "user_2fQ81" }
        });
        let details = WorkflowDetails {
            versions: vec![latest.clone(), parent, first],
            source: "export default async () => ({ closed: 0 });\n".into(),
            tests: vec![
                SuiteEntry {
                    name: "closes nothing without approval".into(),
                    digest: inline_digest.clone(),
                    test: Some(serde_json::from_value(inline).unwrap()),
                    unreadable: None,
                },
                SuiteEntry {
                    name: format!("recorded {RUN_ID}"),
                    digest: recorded_digest.clone(),
                    test: Some(serde_json::from_value(recorded).unwrap()),
                    unreadable: None,
                },
                SuiteEntry {
                    name: "recorded 0193a5c4-0000-7000-8000-000000000000".into(),
                    digest: foreign.clone(),
                    test: None,
                    unreadable: Some(
                        "reading this test needs runs:read in the namespace it was recorded from"
                            .into(),
                    ),
                },
            ],
        };
        let short = |version: &str| version[..12].to_owned();
        assert_eq!(
            describe_workflow_details(&details, &Limits::default()),
            [
                "The workflow triage@4be1c07a9d3e, saved 2026-11-12T10:00:00Z: its latest version, of 3 versions.".to_owned(),
                format!("It derives from the version triage@{}.", short(&parent_version)),
                "Description: Triage the stale issues of a repository, after approval.".to_owned(),
                format!("Input schema: {}", json(&latest.input)),
                format!("Output schema: {}", json(&latest.output)),
                "Signals:".to_owned(),
                format!("- \"approval\": {}", json(&latest.signals["approval"])),
                "Source:".to_owned(),
                "export default async () => ({ closed: 0 });".to_owned(),
                "Its suite, 3 tests, which every version derived from it must pass (give a digest in save_workflow's `tests` or `update` to reuse a test):".to_owned(),
                format!("Test \"closes nothing without approval\" ({inline_digest}):"),
                // `JSON.stringify` keeps the order the server writes a test's fields in.
                "{\"name\":\"closes nothing without approval\",\"input\":{\"repo\":\"acme/api\"},\"world\":{\"tools\":{\"github.list_issues\":[{\"returns\":[{\"number\":4}]}]}},\"signals\":[{\"name\":\"approval\",\"payload\":{\"close\":false}}],\"expect\":{\"output\":{\"closed\":0},\"calls\":[{\"tool\":\"github.close_issue\",\"times\":0}]}}".to_owned(),
                format!(
                    "Test \"recorded {RUN_ID}\" ({recorded_digest}), recorded from run {RUN_ID} of triage@{}:",
                    short(&parent_version)
                ),
                format!(
                    "{{\"name\":\"recorded {RUN_ID}\",\"input\":{{\"repo\":\"acme/api\"}},\"seed\":\"{}\",\"world\":{{\"cassette\":{{\"summary\":\"3 recorded calls matched by_args (github.list_issues ×1, github.close_issue ×2), 1 signal (approval)\"}}}},\"expect\":{{\"status\":\"completed\",\"output\":{{\"closed\":2}}}},\"origin\":{{\"run\":\"{RUN_ID}\",\"version\":\"{parent_version}\",\"namespace\":\"user_2fQ81\"}}}}",
                    "07".repeat(32)
                ),
                format!("Test \"recorded 0193a5c4-0000-7000-8000-000000000000\" ({foreign}): it cannot be read: reading this test needs runs:read in the namespace it was recorded from"),
                "Lineage, newest first:".to_owned(),
                format!("- triage@4be1c07a9d3e, saved 2026-11-12T10:00:00Z, from triage@{}: 3 tests.", short(&parent_version)),
                "  updated \"closes nothing without approval\": the message names the repository".to_owned(),
                "  retired \"asks before closing\": under 5 stale issues, it closes without asking".to_owned(),
                format!(
                    "- triage@{}, saved 2026-11-11T10:00:00Z, from triage@{}: 1 test.",
                    short(&parent_version),
                    short(&first_version)
                ),
                format!("- triage@{}, saved 2026-11-10T10:00:00Z, the first version: 0 tests.", short(&first_version)),
            ]
            .join("\n")
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
            describe_recorded("triage", &recorded, &limits),
            [
                "Recorded 2 tests from the runs of triage, one per path:".to_owned(),
                format!("- {} \"recorded {RUN_ID}\", from run {RUN_ID} (completed): github.list_issues · signal approval · github.close_issue×n → completed", "9f".repeat(32)),
                format!("- {} \"recorded r-2\", from run r-2 (failed): github.list_issues → failed", "8e".repeat(32)),
                "Skipped 1 run:".to_owned(),
                "- r-big: its test would be larger than 4 MiB".to_owned(),
                "They join no suite yet: pass their digests in save_workflow's `tests` to add them to the suite of triage's next version, or { \"from\": \"<digest>\", \"expect\": {…} } to give a failed run the expectation it should have met.".to_owned(),
            ]
            .join("\n")
        );
        let none = RecordedTests {
            tests: vec![],
            skipped: vec![],
        };
        assert_eq!(
            describe_recorded("triage", &none, &limits),
            "No test was recorded: no completed or failed run of triage was selected."
        );
    }

    #[test]
    fn runs_say_which_workflow_they_execute_and_which_run_started_them() {
        let run: Run = serde_json::from_value(json!({
            "id": RUN_ID, "namespace": "user_2fQ81", "status": "completed", "script": "ab".repeat(32),
            "toolset": "t", "epoch": 1, "mode": "live", "labels": {}, "traceId": "0", "historyLen": 4,
            "createdAt": "2026-11-12T10:00:00Z", "updatedAt": "2026-11-12T10:00:01Z",
            "expiresAt": "2026-12-12T10:00:00Z",
            "workflow": { "name": "triage", "version": VERSION },
            "startedBy": { "run": "parent-1", "seq": 2, "detached": true }
        }))
        .unwrap();
        assert_eq!(
            describe_run(&run, &RunExtras::default(), &Limits::default())
                .lines()
                .next()
                .unwrap(),
            format!(
                "Run {RUN_ID} (a run of the workflow triage@4be1c07a9d3e, started by run parent-1, detached) completed."
            )
        );
        let summary: RunSummary = serde_json::from_value(json!({
            "id": "r2", "namespace": "user_2fQ81", "status": "active", "quarantined": false, "script": "s",
            "headline": "", "toolset": "t", "labels": {}, "startedAt": "2026-11-12T10:00:00Z",
            "lastActivityAt": "2026-11-12T10:00:00Z", "toolCalls": 1, "errors": 0, "historyLen": 3,
            "waitingFor": [], "workflow": "triage", "startedBy": "r1"
        }))
        .unwrap();
        assert_eq!(
            describe_runs(&[summary], false, None),
            [
                "1 run, most recent first:",
                "- r2: active, started 2026-11-12T10:00:00Z, 1 tool call, a run of the workflow triage, started by r1",
            ]
            .join("\n")
        );
    }
}
