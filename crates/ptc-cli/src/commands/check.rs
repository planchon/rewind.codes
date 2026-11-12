//! `ptc check script.ts [--toolset id|file]`: type-checks a script (`archi/04-tools.md`,
//! "Type-checking").
//!
//! In this process, without a server: the checks at start of the runtime (syntax, imports,
//! default export), then tsgo and the checker's own rules against the sandbox lib and the
//! declarations of the toolset file, pinned here. A toolset registered on a server lives there,
//! with the result types observed in its runs: the server checks the script against it
//! (`POST /v1/check`).

use std::collections::BTreeMap;

use ptc_api::scripts::{CheckRequest, CheckResult, Diagnostic, Location, Severity};
use ptc_check::TsgoChecker;
use ptc_core::{Script, Toolset};
use ptc_engine::ports::{self, DiagnosticSource, ScriptExecutor};
use ptc_runtime::{SandboxLimits, V8Executor};

use super::Server;
use crate::args::CheckArgs;
use crate::error::CliError;
use crate::exit::Exit;
use crate::input::{self, ToolsetRef};
use crate::output::Console;
use crate::render;
use crate::toolset;

/// The code of a diagnostic from the runtime's checks at start.
const START_CHECK: &str = "ptc/invalid-script";

pub(super) async fn check(
    server: &Server<'_>,
    args: CheckArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let source = input::read_text(&args.script)?;
    let toolset = args.toolset.as_deref().map(ToolsetRef::parse).transpose()?;
    let result = match toolset {
        Some(ToolsetRef::Id(id)) => {
            let client = server.client().await?;
            client
                .check(&CheckRequest {
                    script: source,
                    toolset: Some(id),
                })
                .await?
        }
        Some(ToolsetRef::File(path)) => {
            let toolset = toolset::pin_file(&path).await?;
            check_locally(&source, &toolset).await?
        }
        None => {
            let toolset = Toolset::new([], [])
                .map_err(|error| CliError::internal(format!("the empty toolset: {error}")))?;
            check_locally(&source, &toolset).await?
        }
    };
    if console.json() {
        console.print_json(&result)?;
    } else {
        let file = args.script.display().to_string();
        console.text(&render::diagnostics(&file, &result.diagnostics));
    }
    Ok(if result.ok {
        Exit::Success
    } else {
        Exit::ScriptRefused
    })
}

/// The checks at start and the type-check, in this process.
pub(crate) async fn check_locally(
    source: &str,
    toolset: &Toolset,
) -> Result<CheckResult, CliError> {
    let script = Script::new(source);
    let executor = V8Executor::new(SandboxLimits::default())
        .map_err(|error| CliError::internal(error.to_string()))?;
    let mut diagnostics = Vec::new();
    if let Err(invalid) = executor.compile(&script, toolset) {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: START_CHECK.into(),
            message: invalid.message,
            location: None,
            frame: None,
            hint: None,
        });
    }
    let declarations = ptc_codegen::generate(toolset, &BTreeMap::new());
    let checker = TsgoChecker::new(TsgoChecker::default_cache_dir());
    let found = checker
        .check_source(source, &declarations.typescript)
        .await
        .map_err(|error| CliError::internal(format!("the type-checker failed: {error}")))?;
    diagnostics.extend(found.into_iter().map(diagnostic));
    let ok = diagnostics
        .iter()
        .all(|diagnostic| diagnostic.severity != Severity::Error);
    Ok(CheckResult { ok, diagnostics })
}

/// A diagnostic of the type-checker, as the API writes it.
pub(crate) fn diagnostic(found: ports::Diagnostic) -> Diagnostic {
    Diagnostic {
        severity: match found.severity {
            ports::Severity::Error => Severity::Error,
            ports::Severity::Warning => Severity::Warning,
        },
        code: match &found.source {
            DiagnosticSource::TypeScript { code } => format!("TS{code}"),
            DiagnosticSource::Rule(rule) => format!("ptc/{rule}"),
        },
        message: found.message,
        location: Some(Location {
            line: found.line,
            column: found.column,
            end_line: None,
            end_column: None,
        }),
        frame: (!found.frame.is_empty()).then_some(found.frame),
        hint: found.hint,
    }
}
