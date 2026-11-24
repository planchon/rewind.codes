use std::borrow::Cow;
use std::fs;
use std::io;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rewind_core::Script;
use rewind_engine::ports::{
    ActionDeclarations, CheckerError, Diagnostic, DiagnosticSource, Severity, TypeChecker,
    WorkflowDeclarations,
};
use tokio::sync::{OnceCell, Semaphore};

use crate::error::CheckError;
use crate::rules::{self, Finding};
use crate::sandbox::{self, ACTIONS, ENTRY, FETCH, SCRIPT, WORKFLOW};
use crate::source::{SourceText, blank_directives};
use crate::tsgo::{self, Reported};

/// The first line of the `rewind-actions.d.ts` of an action set that enables the `http` action. The
/// checker then gives tsgo `fetch.d.ts`, and `fetch` is not an absent global.
pub const HTTP_ACTION_REFERENCE: &str = "/// <reference path=\"fetch.d.ts\" />";

/// The declarations of an action set without actions: `rewind:actions` exists, and exports nothing.
const NO_ACTIONS: &str = "declare module \"rewind:actions\" {}\n";

/// `ToolContent`, the name `ActionContent` had, for code already written (`archi/08-decisions.md`,
/// R1): given to tsgo when the action set's declarations have `ActionContent`, and shown nowhere.
const OLD_CONTENT: &str = "declare module \"rewind:tools\" {\n  \
    import type { ActionContent } from \"rewind:actions\";\n  \
    export type ToolContent<T = unknown> = ActionContent<T>;\n}\n";

/// The declarations of an action set that pins no workflow: `rewind:workflows` exists, and exports
/// nothing.
const NO_WORKFLOWS: &str = "\ndeclare module \"rewind:workflows\" {}\n";

/// What a workflow's own script is also checked against: its signals and its entry.
#[derive(Clone, Copy)]
struct Workflow<'a> {
    signals: &'a str,
    entry: &'a str,
}

/// The [`TypeChecker`] of Rewind: tsgo, the native TypeScript 7 compiler, embedded in the
/// binary and driven as a CLI, and the checker's own rules (`archi/04-tools.md`,
/// "Type-checking").
///
/// A check writes the script as `script.ts` in a directory of its own, next to the sandbox lib
/// and the action set's `rewind-actions.d.ts`, and runs tsgo there in strict mode. tsgo is
/// extracted to the cache directory at first use. Diagnostics are written for a model: a location
/// in `script.ts` (columns count characters), the message, a hint for the checker's own rules,
/// and a short code frame. They come sorted by position, and the same script and declarations
/// always give the same diagnostics.
///
/// Checks run at most `concurrency` tsgo processes at a time (the number of CPUs by default),
/// each stopped after the timeout (20 s by default). A check takes about 30 ms.
#[derive(Debug)]
pub struct TsgoChecker {
    cache_dir: PathBuf,
    timeout: Duration,
    executable: OnceCell<PathBuf>,
    slots: Semaphore,
}

impl TsgoChecker {
    /// How long tsgo may take by default. A check takes about 30 ms; types built to explode
    /// can take much longer.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

    /// The diagnostics a check returns at most: the first, by position. A script with errors
    /// on every line would otherwise give millions, and as many allocations.
    pub const MAX_DIAGNOSTICS: usize = 1_000;

    /// A checker that extracts tsgo into `cache_dir` at first use. Nothing is read or written
    /// before.
    pub fn new(cache_dir: impl Into<PathBuf>) -> Self {
        let concurrency = std::thread::available_parallelism().map_or(4, NonZero::get);
        Self {
            cache_dir: cache_dir.into(),
            timeout: Self::DEFAULT_TIMEOUT,
            executable: OnceCell::new(),
            slots: Semaphore::new(concurrency),
        }
    }

    /// `rewind` in the user's cache directory (`~/.cache` on Linux, `~/Library/Caches` on
    /// macOS), or in the system's temporary directory when there is none.
    pub fn default_cache_dir() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("rewind")
    }

    /// Stops tsgo after `timeout`; the check then fails.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Runs at most `checks` tsgo processes at a time; other checks wait their turn.
    pub fn with_concurrency(mut self, checks: usize) -> Self {
        self.slots = Semaphore::new(checks.max(1));
        self
    }

    /// Extracts tsgo into the cache directory unless it is there, and returns its executable.
    /// The first check does it otherwise; a composition root may call it at start.
    pub async fn install(&self) -> Result<&Path, CheckError> {
        self.executable
            .get_or_try_init(|| async {
                let dir = self.cache_dir.clone();
                tokio::task::spawn_blocking(move || tsgo::install(&dir))
                    .await
                    .map_err(|error| CheckError::Install {
                        dir: self.cache_dir.clone(),
                        source: io::Error::other(error),
                    })?
            })
            .await
            .map(PathBuf::as_path)
    }

    /// Type-checks a script against the sandbox lib and `declarations`, the action set's
    /// `rewind-actions.d.ts`, and runs the checker's own rules.
    ///
    /// `declarations` declare the module `rewind:actions` (`declare module "rewind:actions" { …
    /// }`); empty, the module exports nothing. When they start with [`HTTP_ACTION_REFERENCE`], the
    /// action set enables the `http` action, and the script has `fetch`.
    pub async fn check_source(
        &self,
        source: &str,
        declarations: &str,
    ) -> Result<Vec<Diagnostic>, CheckError> {
        self.check_with(source, declarations, None).await
    }

    /// Type-checks a workflow's script as [`check_source`](Self::check_source) does, with the
    /// workflow's own declarations too (`rewind-codegen`'s `workflow_declarations`): its
    /// signals, which type `signal()` and `webhook()`, and `entry.ts`, whose diagnostics are
    /// reported at the script's default export: it does not take the workflow's input, or does
    /// not return its output.
    pub async fn check_workflow_source(
        &self,
        source: &str,
        declarations: &str,
        signals: &str,
        entry: &str,
    ) -> Result<Vec<Diagnostic>, CheckError> {
        self.check_with(source, declarations, Some(Workflow { signals, entry }))
            .await
    }

    async fn check_with(
        &self,
        source: &str,
        declarations: &str,
        workflow: Option<Workflow<'_>>,
    ) -> Result<Vec<Diagnostic>, CheckError> {
        let executable = self.install().await?;
        // Neither the script nor the declarations may add files or libs to the check: the DOM
        // lib would declare what does not exist.
        let (declarations, directives) = blank_directives(declarations);
        let http_action = directives.iter().any(|directive| {
            directive.kind == "path" && matches!(directive.value.as_str(), FETCH | "./fetch.d.ts")
        });
        let mut declarations = if declarations.trim().is_empty() {
            Cow::Borrowed(NO_ACTIONS)
        } else {
            declarations
        };
        if !declarations.contains("declare module \"rewind:workflows\"") {
            declarations.to_mut().push_str(NO_WORKFLOWS);
        }
        let (script, _) = blank_directives(source);
        let (signals, entry) = match workflow {
            Some(workflow) => (
                Some(blank_directives(workflow.signals).0),
                Some(workflow.entry),
            ),
            None => (None, None),
        };
        let extra = |entry: Option<&'_ str>| {
            let mut files = Vec::new();
            if let Some(signals) = &signals {
                files.push((WORKFLOW, signals.to_string()));
            }
            if let Some(entry) = entry {
                files.push((ENTRY, entry.to_owned()));
            }
            files
        };

        let findings = rules::check(source, http_action);
        let reported = self
            .tsgo(
                executable,
                &script,
                &declarations,
                http_action,
                extra(entry),
            )
            .await?;
        let (in_script, rest): (Vec<_>, Vec<_>) = reported.into_iter().partition(
            |reported| matches!(&reported.location, Some((file, _, _)) if file == SCRIPT),
        );
        let (in_entry, elsewhere): (Vec<_>, Vec<_>) = rest.into_iter().partition(
            |reported| matches!(&reported.location, Some((file, _, _)) if file == ENTRY),
        );
        if !elsewhere.is_empty() {
            // Whether the script caused them: checked again with an empty script, and without
            // the entry, which needs the script, the declarations must have no diagnostic.
            let alone = self
                .tsgo(
                    executable,
                    "export {};\n",
                    &declarations,
                    http_action,
                    extra(None),
                )
                .await?;
            if !alone.is_empty() {
                return Err(CheckError::Declarations(render(&alone)));
            }
        }
        let mut diagnostics = diagnostics(source, &findings, in_script, elsewhere);
        diagnostics.extend(entry_diagnostics(source, in_entry));
        diagnostics.sort_by(|a, b| key(a).cmp(&key(b)));
        diagnostics.dedup();
        diagnostics.truncate(Self::MAX_DIAGNOSTICS);
        Ok(diagnostics)
    }

    /// Runs tsgo on a script and declarations, in a directory of their own.
    async fn tsgo(
        &self,
        executable: &Path,
        script: &str,
        declarations: &str,
        http_action: bool,
        extra: Vec<(&'static str, String)>,
    ) -> Result<Vec<Reported>, CheckError> {
        let _slot = self.slots.acquire().await.map_err(|_| CheckError::Failed {
            status: "not started".into(),
            output: "the checker was closed".into(),
        })?;
        let dir = tempfile::Builder::new()
            .prefix("rewind-check-")
            .tempdir()
            .map_err(CheckError::Files)?;
        let old_content = declarations
            .contains("export interface ActionContent<")
            .then_some(("old-content.d.ts", OLD_CONTENT));
        let files = sandbox::files(http_action)
            .into_iter()
            .chain(old_content)
            .chain([(ACTIONS, declarations), (SCRIPT, script)]);
        for (name, text) in files {
            fs::write(dir.path().join(name), text).map_err(CheckError::Files)?;
        }
        for (name, text) in extra {
            fs::write(dir.path().join(name), text).map_err(CheckError::Files)?;
        }
        let output = tsgo::run(executable, dir.path(), self.timeout).await?;
        let (located, global): (Vec<_>, Vec<_>) = output
            .diagnostics
            .into_iter()
            .partition(|reported| reported.location.is_some());
        if !global.is_empty() {
            // About the configuration, the lib files or the options: never about the script.
            return Err(CheckError::Failed {
                status: "diagnostics without a location".into(),
                output: render(&global),
            });
        }
        Ok(located)
    }
}

impl TypeChecker for TsgoChecker {
    async fn check(
        &self,
        script: &Script,
        declarations: &ActionDeclarations,
    ) -> Result<Vec<Diagnostic>, CheckerError> {
        Ok(self
            .check_source(script.source(), &declarations.typescript)
            .await?)
    }

    async fn check_workflow(
        &self,
        script: &Script,
        declarations: &ActionDeclarations,
        workflow: &WorkflowDeclarations,
    ) -> Result<Vec<Diagnostic>, CheckerError> {
        Ok(self
            .check_workflow_source(
                script.source(),
                &declarations.typescript,
                &workflow.signals,
                &workflow.entry,
            )
            .await?)
    }
}

/// What tsgo reported in a workflow's `entry.ts`: the script's default export does not take
/// the workflow's input or does not return its output. Reported at the default export.
fn entry_diagnostics(source: &str, in_entry: Vec<Reported>) -> Vec<Diagnostic> {
    let text = SourceText::new(source);
    let (start, end) = rules::default_export(source).unwrap_or((0, 0));
    let (line, column) = text.position(start);
    in_entry
        .into_iter()
        .map(|reported| Diagnostic {
            line,
            column,
            severity: severity(&reported),
            source: DiagnosticSource::TypeScript {
                code: reported.code,
            },
            message: format!(
                "the default export does not match the workflow's schemas: {}",
                reported.message
            ),
            hint: Some(
                "the default export must take the workflow's input and return its output, as \
                 its input and output schemas declare them"
                    .into(),
            ),
            frame: text.frame(start, end.min(text.token_end(start).max(start))),
        })
        .collect()
}

/// The diagnostics of a check, sorted by position.
fn diagnostics(
    source: &str,
    findings: &[Finding],
    in_script: Vec<Reported>,
    elsewhere: Vec<Reported>,
) -> Vec<Diagnostic> {
    let text = SourceText::new(source);
    let mut diagnostics: Vec<Diagnostic> = findings
        .iter()
        .map(|finding| {
            let (line, column) = text.position(finding.start);
            Diagnostic {
                line,
                column,
                severity: Severity::Error,
                source: DiagnosticSource::Rule(finding.rule.to_owned()),
                message: finding.message.clone(),
                hint: finding.hint.clone(),
                frame: text.frame(finding.start, finding.end),
            }
        })
        .collect();
    for reported in in_script {
        let Some((_, line, column)) = reported.location else {
            continue;
        };
        let start = text.offset_of_utf16(line, column);
        // An absent global: the rule's diagnostic says what to use instead.
        let replaced = findings.iter().any(|finding| {
            finding.start == start
                && finding
                    .global
                    .as_ref()
                    .is_some_and(|name| reported.message.contains(&format!("'{name}'")))
        });
        if replaced {
            continue;
        }
        let (line, column) = text.position(start);
        diagnostics.push(Diagnostic {
            line,
            column,
            severity: severity(&reported),
            source: DiagnosticSource::TypeScript {
                code: reported.code,
            },
            message: reported.message,
            hint: None,
            frame: text.frame(start, text.token_end(start)),
        });
    }
    // Caused by the script, but found in another file: a `declare global` that conflicts with
    // the sandbox's globals. tsgo reports most conflicts on both sides; the other side is
    // shown, at the start of the script with where tsgo found it, only when the script has no
    // diagnostic of its own.
    let own = diagnostics
        .iter()
        .any(|diagnostic| matches!(diagnostic.source, DiagnosticSource::TypeScript { .. }));
    for reported in elsewhere.into_iter().filter(|_| !own) {
        let Some((file, line, column)) = &reported.location else {
            continue;
        };
        diagnostics.push(Diagnostic {
            line: 1,
            column: 1,
            severity: severity(&reported),
            source: DiagnosticSource::TypeScript {
                code: reported.code,
            },
            message: format!("{file}:{line}:{column}: {}", reported.message),
            hint: None,
            frame: String::new(),
        });
    }
    diagnostics.sort_by(|a, b| key(a).cmp(&key(b)));
    diagnostics.dedup();
    diagnostics.truncate(TsgoChecker::MAX_DIAGNOSTICS);
    diagnostics
}

fn severity(reported: &Reported) -> Severity {
    if reported.warning {
        Severity::Warning
    } else {
        Severity::Error
    }
}

/// The order of diagnostics: position, then tsgo's before the rules', then code and message.
fn key(diagnostic: &Diagnostic) -> (u32, u32, u8, u32, &str, &str) {
    let (kind, code, rule) = match &diagnostic.source {
        DiagnosticSource::TypeScript { code } => (0, *code, ""),
        DiagnosticSource::Rule(rule) => (1, 0, rule.as_str()),
    };
    (
        diagnostic.line,
        diagnostic.column,
        kind,
        code,
        rule,
        &diagnostic.message,
    )
}

/// Diagnostics as tsgo prints them, one per line.
fn render(reported: &[Reported]) -> String {
    reported
        .iter()
        .map(|reported| match &reported.location {
            Some((file, line, column)) => format!(
                "{file}({line},{column}): TS{}: {}",
                reported.code, reported.message
            ),
            None => format!("TS{}: {}", reported.code, reported.message),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::ABSENT_GLOBAL;

    fn reported(file: &str, line: u32, column: u32, code: u32, message: &str) -> Reported {
        Reported {
            location: Some((file.to_owned(), line, column)),
            warning: false,
            code,
            message: message.to_owned(),
        }
    }

    fn summary(diagnostics: &[Diagnostic]) -> Vec<(u32, u32, String)> {
        diagnostics
            .iter()
            .map(|diagnostic| {
                (
                    diagnostic.line,
                    diagnostic.column,
                    diagnostic.message.clone(),
                )
            })
            .collect()
    }

    const SOURCE: &str = "const a = process.env;\nconst b: number = \"x\";\n";

    fn absent_process() -> Finding {
        Finding {
            start: 10,
            end: 17,
            rule: ABSENT_GLOBAL,
            message: "`process` does not exist in Rewind scripts.".into(),
            hint: Some("Pass values through the input.".into()),
            global: Some("process".into()),
        }
    }

    #[test]
    fn diagnostics_are_sorted_and_absent_globals_replace_tsgo() {
        let diagnostics = diagnostics(
            SOURCE,
            &[absent_process()],
            vec![
                reported(
                    SCRIPT,
                    2,
                    7,
                    2322,
                    "Type 'string' is not assignable to type 'number'.",
                ),
                reported(
                    SCRIPT,
                    1,
                    11,
                    2591,
                    "Cannot find name 'process'. Do you need to install type definitions for node?",
                ),
                reported(SCRIPT, 1, 11, 9999, "Another problem with 'env'."),
            ],
            Vec::new(),
        );
        assert_eq!(
            summary(&diagnostics),
            [
                (1, 11, "Another problem with 'env'.".into()),
                (1, 11, "`process` does not exist in Rewind scripts.".into()),
                (
                    2,
                    7,
                    "Type 'string' is not assignable to type 'number'.".into()
                ),
            ]
        );
        assert_eq!(
            diagnostics[1].frame,
            "1 | const a = process.env;\n  |           ^^^^^^^\n2 | const b: number = \"x\";"
        );
        assert_eq!(
            diagnostics[2].frame,
            "1 | const a = process.env;\n2 | const b: number = \"x\";\n  |       ^"
        );
    }

    #[test]
    fn diagnostics_of_other_files_show_only_when_the_script_has_none() {
        let elsewhere = || {
            vec![reported(
                "globals.d.ts",
                13,
                3,
                2300,
                "Duplicate identifier 'log'.",
            )]
        };
        let alone = diagnostics(SOURCE, &[], Vec::new(), elsewhere());
        assert_eq!(
            summary(&alone),
            [(
                1,
                1,
                "globals.d.ts:13:3: Duplicate identifier 'log'.".into()
            )]
        );
        assert_eq!(alone[0].frame, "");
        let with_own = diagnostics(
            SOURCE,
            &[],
            vec![reported(SCRIPT, 2, 7, 2300, "Duplicate identifier 'log'.")],
            elsewhere(),
        );
        assert_eq!(
            summary(&with_own),
            [(2, 7, "Duplicate identifier 'log'.".into())]
        );
    }
}
