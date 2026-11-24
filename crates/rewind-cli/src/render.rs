//! Text output: what a person reads when `--json` is not given.

use std::fmt::Write as _;

use rewind_api::common::{Payload, RunStatus};
use rewind_api::runs::{Run, RunSummary, Waiting};
use rewind_api::scripts::{Diagnostic, Severity};

/// A run's state: its status, then what it returned, how it failed, or what it waits for.
pub(crate) fn run(run: &Run) -> String {
    let mut text = String::new();
    for log in &run.logs {
        let level = format!("{:?}", log.level).to_lowercase();
        let _ = writeln!(text, "[{level}] {}", log.message);
    }
    let _ = write!(text, "run {} {}", run.handle, run.status);
    if let Some(parent) = &run.source {
        let _ = write!(text, " (fork of {}", parent.handle);
        if let Some(at) = parent.at {
            let _ = write!(text, " at event {at}");
        }
        text.push(')');
    }
    text.push('\n');
    if let Some(quarantine) = &run.quarantine {
        let cause = format!("{:?}", quarantine.cause);
        let _ = writeln!(
            text,
            "quarantined ({}): {}",
            snake_case(&cause),
            quarantine.reason
        );
        let _ = writeln!(
            text,
            "  `rewind release {}` retries it, `rewind release {} --give-up` aborts it",
            run.handle, run.handle
        );
    }
    match run.status {
        RunStatus::Completed => {
            if let Some(output) = &run.output {
                let _ = writeln!(text, "output: {}", payload(output));
            }
        }
        RunStatus::Failed | RunStatus::Cancelled => {
            if let Some(error) = &run.error {
                let _ = writeln!(text, "{}: {}", error.name, error.message);
                if let Some(location) = &error.location {
                    let _ = writeln!(text, "  at {location}");
                }
                if let Some(frame) = &error.frame {
                    let _ = writeln!(text, "{}", indent(frame));
                }
                if let Some(call) = &error.failed_call {
                    let kind = format!("{:?}", call.error.kind);
                    let _ = writeln!(
                        text,
                        "  failed call #{} {}({}): {}: {}",
                        call.seq,
                        call.action,
                        payload(&call.args),
                        snake_case(&kind),
                        call.error.message
                    );
                }
            }
        }
        RunStatus::Aborted => {
            if let Some(abort) = &run.abort {
                let kind = format!("{:?}", abort.kind);
                let _ = writeln!(text, "aborted ({}): {}", snake_case(&kind), abort.message);
            }
        }
        RunStatus::Pending | RunStatus::Active => {
            if !run.waiting_for.is_empty() {
                text.push_str("waiting for:\n");
                for waiting in &run.waiting_for {
                    let _ = writeln!(text, "  - {}", self::waiting(waiting));
                }
            }
        }
    }
    text
}

fn waiting(waiting: &Waiting) -> String {
    match waiting {
        Waiting::Call {
            seq,
            action,
            args,
            state,
            parked,
            delivery,
            callsite,
            call_id,
        } => {
            let state = snake_case(&format!("{state:?}"));
            let mut text = format!(
                "call #{seq} {action}({}): {state}, delivery {delivery}",
                payload(args)
            );
            if let Some(parked) = parked {
                let _ = write!(text, " ({})", snake_case(&format!("{parked:?}")));
            }
            if let Some(callsite) = callsite {
                let _ = write!(text, " at {callsite}");
            }
            let _ = write!(text, " [call {call_id}]");
            text
        }
        Waiting::Timer {
            seq,
            fire_at,
            callsite,
        } => {
            let mut text = format!("timer #{seq}, fires at {fire_at}");
            if let Some(callsite) = callsite {
                let _ = write!(text, " ({callsite})");
            }
            text
        }
        Waiting::Signal {
            name,
            timeout_at,
            callsite,
        } => {
            let mut text = format!("signal {name:?}");
            if let Some(timeout_at) = timeout_at {
                let _ = write!(text, ", until {timeout_at}");
            }
            if let Some(callsite) = callsite {
                let _ = write!(text, " ({callsite})");
            }
            text
        }
    }
}

/// A payload: its value when the answer has it, else its preview, size and digest.
pub(crate) fn payload(payload: &Payload) -> String {
    match &payload.value {
        Some(value) => value.to_string(),
        None => format!(
            "{} ({} bytes, `rewind transcript --event` shows it; digest {})",
            payload.preview, payload.bytes, payload.digest
        ),
    }
}

/// The table of `rewind runs`.
pub(crate) fn runs(runs: &[RunSummary]) -> String {
    if runs.is_empty() {
        return "no runs\n".to_owned();
    }
    let mut rows = vec![[
        "RUN".to_owned(),
        "STATUS".to_owned(),
        "STARTED".to_owned(),
        "CALLS".to_owned(),
        "ERRORS".to_owned(),
        "SCRIPT".to_owned(),
        "LABELS".to_owned(),
    ]];
    for run in runs {
        let mut status = run.status.to_string();
        if run.quarantined {
            status.push_str(" (quarantined)");
        }
        if let Some(error) = &run.error {
            let _ = write!(status, " {error}");
        }
        rows.push([
            run.handle.clone(),
            status,
            run.started_at.to_string(),
            run.calls.to_string(),
            run.errors.to_string(),
            // The code's title, else its first line.
            shorten(
                run.script_title.as_deref().unwrap_or(&run.headline),
                HEADLINE_CHARS,
            ),
            run.labels
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect::<Vec<_>>()
                .join(","),
        ]);
    }
    table(&rows)
}

/// Characters of a code's title or first line in `rewind runs`.
const HEADLINE_CHARS: usize = 40;

fn shorten(text: &str, chars: usize) -> String {
    match text.char_indices().nth(chars) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_owned(),
    }
}

/// Rows in aligned columns, the last one unpadded.
pub(crate) fn table<const N: usize>(rows: &[[String; N]]) -> String {
    let mut widths = [0usize; N];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut text = String::new();
    for row in rows {
        let mut line = String::new();
        for (column, cell) in row.iter().enumerate() {
            if column + 1 == N {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<width$}  ", width = widths[column]);
            }
        }
        text.push_str(line.trim_end());
        text.push('\n');
    }
    text
}

/// Diagnostics as compiler output, located in the code's own file.
pub(crate) fn diagnostics(file: &str, diagnostics: &[Diagnostic]) -> String {
    let mut text = String::new();
    for diagnostic in diagnostics {
        match &diagnostic.location {
            Some(location) => {
                let _ = write!(text, "{file}:{}:{}", location.line, location.column);
            }
            None => text.push_str(file),
        }
        let _ = writeln!(
            text,
            " {} {}: {}",
            diagnostic.severity.as_str(),
            diagnostic.code,
            diagnostic.message
        );
        if let Some(frame) = &diagnostic.frame {
            let _ = writeln!(text, "{frame}");
        }
        if let Some(hint) = &diagnostic.hint {
            let _ = writeln!(text, "hint: {hint}");
        }
    }
    let errors = diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == Severity::Error)
        .count();
    let warnings = diagnostics.len() - errors;
    match (errors, warnings) {
        (0, 0) => {
            let _ = writeln!(text, "{file}: no problems");
        }
        _ => {
            let _ = writeln!(
                text,
                "{file}: {errors} error{}, {warnings} warning{}",
                plural(errors),
                plural(warnings)
            );
        }
    }
    text
}

fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `NonDeterminism` → `non_determinism`, as the API writes enumerated values.
fn snake_case(name: &str) -> String {
    let mut text = String::new();
    for (index, char) in name.chars().enumerate() {
        if char.is_uppercase() {
            if index > 0 {
                text.push('_');
            }
            text.extend(char.to_lowercase());
        } else {
            text.push(char);
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use rewind_api::common::{Callsite, Labels, RunMode};
    use rewind_api::runs::{Quarantine, QuarantineCause, RunError};
    use rewind_api::scripts::Location;
    use serde_json::json;

    use super::*;

    fn active_run() -> Run {
        Run {
            id: "r1".into(),
            handle: "local/script/06bqx3k9f2mt".into(),
            namespace: "local".parse().unwrap(),
            status: RunStatus::Active,
            quarantine: None,
            code_digest: "s".into(),
            script_title: None,
            action_set: "t".into(),
            epoch: 1,
            mode: RunMode::Live,
            labels: Labels::new(),
            secrets: Default::default(),
            trace_id: "0".repeat(32),
            source: None,
            workflow: None,
            started_by: None,
            input: Some(Payload::of(json!({}), 1024)),
            output: None,
            error: None,
            abort: None,
            waiting_for: vec![Waiting::Signal {
                name: "approval".into(),
                timeout_at: None,
                callsite: Some(Callsite { line: 4, column: 9 }),
            }],
            logs: Vec::new(),
            deliveries: Vec::new(),
            history_len: 3,
            created_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            updated_at: "2026-01-01T00:00:00Z".parse().unwrap(),
            ended_at: None,
            expires_at: "2026-01-31T00:00:00Z".parse().unwrap(),
        }
    }

    #[test]
    fn a_waiting_run_says_what_it_waits_for() {
        let text = run(&active_run());
        assert_eq!(
            text,
            "run local/script/06bqx3k9f2mt active\nwaiting for:\n  - signal \"approval\" (workflow.ts:4:9)\n"
        );
    }

    #[test]
    fn a_failed_or_quarantined_run_says_why() {
        let mut failed = active_run();
        failed.status = RunStatus::Failed;
        failed.waiting_for.clear();
        failed.error = Some(RunError {
            name: "TypeError".into(),
            message: "x is undefined".into(),
            stack: None,
            location: Some(Callsite { line: 7, column: 3 }),
            frame: Some("7 | x.y\n  | ^".into()),
            failed_call: None,
        });
        let text = run(&failed);
        assert!(
            text.contains(
                "run local/script/06bqx3k9f2mt failed\nTypeError: x is undefined\n  at workflow.ts:7:3\n  7 | x.y"
            )
        );

        let mut quarantined = active_run();
        quarantined.quarantine = Some(Quarantine {
            cause: QuarantineCause::NonDeterminism,
            reason: "event 4 differs".into(),
        });
        assert!(run(&quarantined).contains("quarantined (non_determinism): event 4 differs"));
    }

    #[test]
    fn diagnostics_read_like_a_compiler() {
        let text = diagnostics(
            "triage.ts",
            &[Diagnostic {
                severity: Severity::Error,
                code: "TS2322".into(),
                message: "Type 'number' is not assignable to type 'string'.".into(),
                location: Some(Location {
                    line: 3,
                    column: 28,
                    end_line: None,
                    end_column: None,
                }),
                frame: Some("3 | x".into()),
                hint: None,
            }],
        );
        assert_eq!(
            text,
            "triage.ts:3:28 error TS2322: Type 'number' is not assignable to type 'string'.\n3 | x\ntriage.ts: 1 error, 0 warnings\n"
        );
        assert_eq!(diagnostics("a.ts", &[]), "a.ts: no problems\n");
    }

    #[test]
    fn tables_align_their_columns() {
        let rows = [
            ["A".to_owned(), "LONGER".to_owned(), "x".to_owned()],
            ["abc".to_owned(), "b".to_owned(), "y".to_owned()],
        ];
        assert_eq!(table(&rows), "A    LONGER  x\nabc  b       y\n");
        assert_eq!(snake_case("NonDeterminism"), "non_determinism");
        assert_eq!(shorten("abcdef", 3), "abc…");
        assert_eq!(shorten("abc", 3), "abc");
    }
}
