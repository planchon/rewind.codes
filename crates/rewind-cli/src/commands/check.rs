//! `rewind check workflow.ts [--action-set id|file]`: type-checks a code (`archi/04-tools.md`,
//! "Type-checking").
//!
//! In this process, without a server: the checks at start of the runtime (syntax, imports,
//! default export), then tsgo and the checker's own rules against the sandbox lib and the
//! declarations of the action set file, pinned here. An action set registered on a server lives
//! there, with the result types observed in its runs: the server checks the code against it
//! (`POST /v1/check`).

use std::collections::BTreeMap;

use rewind_api::scripts::{CheckRequest, CheckResult, Diagnostic, Location, Severity};
use rewind_check::TsgoChecker;
use rewind_core::{ActionSet, Code};
use rewind_engine::ports::{self, CodeExecutor, DiagnosticSource};
use rewind_runtime::{SandboxLimits, V8Executor};

use super::Server;
use crate::action_set;
use crate::args::CheckArgs;
use crate::error::CliError;
use crate::exit::Exit;
use crate::input::{self, ActionSetRef};
use crate::output::Console;
use crate::render;

/// The code of a diagnostic from the runtime's checks at start.
const START_CHECK: &str = "rewind/invalid-code";

pub(super) async fn check(
    server: &Server<'_>,
    args: CheckArgs,
    console: &mut Console,
) -> Result<Exit, CliError> {
    let source = input::read_text(&args.code)?;
    let action_set = args
        .action_set
        .as_deref()
        .map(ActionSetRef::parse)
        .transpose()?;
    let result = match action_set {
        Some(ActionSetRef::Id(id)) => {
            let client = server.client().await?;
            client
                .check(&CheckRequest {
                    code: source,
                    action_set: Some(id),
                })
                .await?
        }
        Some(ActionSetRef::File(path)) => {
            let action_set = action_set::pin_file(&path).await?;
            check_locally(&source, &action_set).await?
        }
        None => {
            let action_set = ActionSet::new([], [])
                .map_err(|error| CliError::internal(format!("the empty action set: {error}")))?;
            check_locally(&source, &action_set).await?
        }
    };
    if console.json() {
        console.print_json(&result)?;
    } else {
        let file = args.code.display().to_string();
        console.text(&render::diagnostics(&file, &result.diagnostics));
    }
    Ok(if result.ok {
        Exit::Success
    } else {
        Exit::CodeRefused
    })
}

/// The checks at start and the type-check, in this process.
pub(crate) async fn check_locally(
    source: &str,
    action_set: &ActionSet,
) -> Result<CheckResult, CliError> {
    let code = Code::new(source);
    let executor = V8Executor::new(SandboxLimits::default())
        .map_err(|error| CliError::internal(error.to_string()))?;
    let mut diagnostics = Vec::new();
    if let Err(invalid) = executor.compile(&code, action_set) {
        diagnostics.push(Diagnostic {
            severity: Severity::Error,
            code: START_CHECK.into(),
            message: invalid.message,
            location: None,
            frame: None,
            hint: None,
        });
    }
    let declarations = rewind_codegen::generate(action_set, &BTreeMap::new());
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
            DiagnosticSource::Rule(rule) => format!("rewind/{rule}"),
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
