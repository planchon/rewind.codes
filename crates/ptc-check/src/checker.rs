use std::borrow::Cow;
use std::fs;
use std::io;
use std::num::NonZero;
use std::path::{Path, PathBuf};
use std::time::Duration;

use ptc_core::Script;
use ptc_engine::ports::{
    CheckerError, Diagnostic, DiagnosticSource, Severity, ToolDeclarations, TypeChecker,
};
use tokio::sync::{OnceCell, Semaphore};

use crate::error::CheckError;
use crate::rules::{self, Finding};
use crate::sandbox::{self, FETCH, SCRIPT, TOOLS};
use crate::source::{SourceText, blank_directives};
use crate::tsgo::{self, Reported};

/// The first line of the `ptc-tools.d.ts` of a toolset that enables the `http` tool. The
/// checker then gives tsgo `fetch.d.ts`, and `fetch` is not an absent global.
pub const HTTP_TOOL_REFERENCE: &str = "/// <reference path=\"fetch.d.ts\" />";

/// The declarations of a toolset without tools: `ptc:tools` exists, and exports nothing.
const NO_TOOLS: &str = "declare module \"ptc:tools\" {}\n";

/// The [`TypeChecker`] of ai-ptc: tsgo, the native TypeScript 7 compiler, embedded in the
/// binary and driven as a CLI, and the checker's own rules (`archi/04-tools.md`,
/// "Type-checking").
///
/// A check writes the script as `script.ts` in a directory of its own, next to the sandbox lib
/// and the toolset's `ptc-tools.d.ts`, and runs tsgo there in strict mode. tsgo is extracted to
/// the cache directory at first use. Diagnostics are written for a model: a location in
/// `script.ts` (columns count characters), the message, a hint for the checker's own rules,
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

    /// `ai-ptc` in the user's cache directory (`~/.cache` on Linux, `~/Library/Caches` on
    /// macOS), or in the system's temporary directory when there is none.
    pub fn default_cache_dir() -> PathBuf {
        dirs::cache_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("ai-ptc")
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

    /// Type-checks a script against the sandbox lib and `declarations`, the toolset's
    /// `ptc-tools.d.ts`, and runs the checker's own rules.
    ///
    /// `declarations` declare the module `ptc:tools` (`declare module "ptc:tools" { … }`);
    /// empty, the module exports nothing. When they start with [`HTTP_TOOL_REFERENCE`], the
    /// toolset enables the `http` tool, and the script has `fetch`.
    pub async fn check_source(
        &self,
        source: &str,
        declarations: &str,
    ) -> Result<Vec<Diagnostic>, CheckError> {
        let executable = self.install().await?;
        // Neither the script nor the declarations may add files or libs to the check: the DOM
        // lib would declare what does not exist.
        let (declarations, directives) = blank_directives(declarations);
        let http_tool = directives.iter().any(|directive| {
            directive.kind == "path" && matches!(directive.value.as_str(), FETCH | "./fetch.d.ts")
        });
        let declarations = if declarations.trim().is_empty() {
            Cow::Borrowed(NO_TOOLS)
        } else {
            declarations
        };
        let (script, _) = blank_directives(source);

        let findings = rules::check(source, http_tool);
        let reported = self
            .tsgo(executable, &script, &declarations, http_tool)
            .await?;
        let (in_script, elsewhere): (Vec<_>, Vec<_>) = reported.into_iter().partition(
            |reported| matches!(&reported.location, Some((file, _, _)) if file == SCRIPT),
        );
        if !elsewhere.is_empty() {
            // Whether the script caused them: checked again with an empty script, the
            // declarations must have no diagnostic.
            let alone = self
                .tsgo(executable, "export {};\n", &declarations, http_tool)
                .await?;
            if !alone.is_empty() {
                return Err(CheckError::Declarations(render(&alone)));
            }
        }
        Ok(diagnostics(source, &findings, in_script, elsewhere))
    }

    /// Runs tsgo on a script and declarations, in a directory of their own.
    async fn tsgo(
        &self,
        executable: &Path,
        script: &str,
        declarations: &str,
        http_tool: bool,
    ) -> Result<Vec<Reported>, CheckError> {
        let _slot = self.slots.acquire().await.map_err(|_| CheckError::Failed {
            status: "not started".into(),
            output: "the checker was closed".into(),
        })?;
        let dir = tempfile::Builder::new()
            .prefix("ptc-check-")
            .tempdir()
            .map_err(CheckError::Files)?;
        let files = sandbox::files(http_tool)
            .into_iter()
            .chain([(TOOLS, declarations), (SCRIPT, script)]);
        for (name, text) in files {
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
        declarations: &ToolDeclarations,
    ) -> Result<Vec<Diagnostic>, CheckerError> {
        Ok(self
            .check_source(script.source(), &declarations.typescript)
            .await?)
    }
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
            message: "`process` does not exist in ai-ptc scripts.".into(),
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
                (1, 11, "`process` does not exist in ai-ptc scripts.".into()),
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
